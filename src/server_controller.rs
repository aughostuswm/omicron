/*!
 * Facilities for interacting with Server Controllers.  See RFD 48.
 */

use crate::api_error::ApiError;
use crate::api_model::ApiInstance;
use crate::api_model::ApiInstanceRuntimeState;
use crate::api_model::ApiInstanceRuntimeStateParams;
use crate::api_model::ApiInstanceState;
use crate::api_model::ApiResourceType;
use crate::controller::ControllerScApi;
use async_trait::async_trait;
use chrono::Utc;
use futures::channel::mpsc::Receiver;
use futures::channel::mpsc::Sender;
use futures::lock::Mutex;
use futures::stream::StreamExt;
use slog::Logger;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/**
 * `ServerController` is a handle for the software service running on a compute
 * server that manages the control plane on that server.  The current
 * implementation simulates a server directly in this program.
 *
 * **It's important to be careful about the interface exposed by this struct.**
 * The intent is for it to eventually be implemented using requests to a remote
 * server.  The tighter the coupling that exists now, the harder this will be to
 * move later.
 */
pub struct ServerController {
    /** unique id for this server */
    pub id: Uuid,

    /** indicates how to simulate instance transitions */
    pub sim_mode: ServerControllerSimMode,

    /** handle for the internal control plane API */
    ctlsc: ControllerScApi,
    /** debug log */
    log: Logger,
    /** collection of simulated instances, indexed by instance uuid */
    instances: Mutex<BTreeMap<Uuid, SimInstance>>,
}

#[derive(Copy, Clone)]
pub enum ServerControllerSimMode {
    Auto,
    Api,
}

impl ServerController {
    /** Constructs a simulated ServerController with the given uuid. */
    pub fn new_simulated_with_id(
        id: &Uuid,
        sim_mode: ServerControllerSimMode,
        log: Logger,
        ctlsc: ControllerScApi,
    ) -> ServerController {
        info!(log, "created server controller");

        ServerController {
            id: id.clone(),
            sim_mode,
            log,
            ctlsc,
            instances: Mutex::new(BTreeMap::new()),
        }
    }

    /**
     * Idempotently ensures that the given API Instance (described by
     * `api_instance`) exists on this server in the given runtime state
     * (described by `target`).
     */
    pub async fn instance_ensure(
        self: &Arc<Self>,
        api_instance: Arc<ApiInstance>,
        target: &ApiInstanceRuntimeStateParams,
    ) -> Result<ApiInstanceRuntimeState, ApiError> {
        let id = api_instance.identity.id.clone();
        let mut instances = self.instances.lock().await;
        let maybe_current_instance = instances.remove(&id);
        let mut instance = {
            if let Some(current_instance) = maybe_current_instance {
                current_instance
            } else {
                /* Create a new Instance. */
                let idc = id.clone();
                let log = self.log.new(o!("instance_id" => idc.to_string()));

                if let ServerControllerSimMode::Auto = self.sim_mode {
                    let (instance, rx) = SimInstance::new_simulated_auto(
                        &api_instance.runtime,
                        log,
                    );
                    let selfc = Arc::clone(&self);
                    tokio::spawn(async move {
                        selfc.instance_sim(idc, rx).await;
                    });
                    instance
                } else {
                    SimInstance::new_simulated_explicit(
                        &api_instance.runtime,
                        log,
                    )
                }
            }
        };

        instance.transition(target);
        let after = instance.current_run_state.clone();
        instances.insert(id.clone(), instance);
        Ok(after)
    }

    /**
     * Non-idempotent call to reboot an instance.  The semantics of a reboot are
     * that the system will be stopped and begin a fresh boot process that
     * begins _after_ this call is made.  Think of the intended use-case of
     * having changed the instance's image or configuration and then rebooting
     * it to pick up that config.  It's important that the boot process begin
     * after this call completes -- otherwise, you couldn't be sure that the
     * boot process picked up your expected changes.  However, it's not
     * important that the stopping process begins after this call is made.  In
     * particular, if we're currently in the "stopping" phase of a previous
     * reboot request, we don't have to do anything at all here because by
     * definition there will be a new boot process started after this call
     * completes.
     *
     * Intuitively, we want the above to hold as long as the user's desired
     * state for the instance is running.  Otherwise, the request is invalid --
     * you can't reboot an instance that's stopped.
     *
     * Putting these requirements together, this means:
     *
     * * If the instance is running, we'll stop it and then start it again.
     * * If the instance is starting, we will begin stopping it immediately and
     *   then start it again.
     * * If the instance is stopped, this call is not valid.
     *
     * What if the instance is stopping?  It depends.  If it's going to come to
     * rest in a stopped state, then the call should be invalid -- this is just
     * like the case of trying to reboot a stopped instance.  On the other hand,
     * if the instance is currently stopping only as part of a previous reboot
     * operation, we should allow this request -- and simply do nothing (as
     * explained above).
     */
    pub async fn instance_reboot(
        self: &Arc<Self>,
        api_instance: Arc<ApiInstance>,
    ) -> Result<ApiInstanceRuntimeState, ApiError> {
        /* TODO-cleanup This implementation leaves much to be desired. */
        let id = api_instance.identity.id.clone();
        let mut instances = self.instances.lock().await;
        let mut instance = instances.remove(&id).ok_or_else(|| {
            ApiError::not_found_by_id(ApiResourceType::Instance, &id)
        })?;

        let current_run_state = &instance.current_run_state.run_state;
        let target_run_state = instance.requested_run_state.as_ref();

        /*
         * When we first see a "Creating" instance in this module we always move
         * it immediately to "Running", and there's no way to transition back to
         * "Creating".  So we should never see "Creating" here.
         */
        assert_ne!(*current_run_state, ApiInstanceState::Creating);

        /*
         * Reboot is not allowed if:
         *
         * - we're in a stopped state (which are all at-rest states)
         * - we're "Stopping" en route to anything other than "Stopped"
         *   (because these are all terminal stopped states)
         * - we're "Stopping" en route to "Stopped" and it's not because we're
         *   currentely processing a previous reboot request
         */
        let rv = if current_run_state.is_stopped()
            || (*current_run_state == ApiInstanceState::Stopping
                && (target_run_state.unwrap().run_state
                    != ApiInstanceState::Stopped
                    || !instance.reboot_in_progress))
        {
            let message = format!(
                "cannot reboot instance in state \"{}\"",
                current_run_state
            );
            warn!(instance.log, "reboot: request failed: {}", message);
            Err(ApiError::InvalidRequest {
                message,
            })
        } else if *current_run_state == ApiInstanceState::Starting
            || *current_run_state == ApiInstanceState::Running
        {
            /* Immediately begin the reboot. */
            info!(instance.log, "reboot: starting");
            instance.reboot_in_progress = true;
            instance.transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Stopped,
            });
            Ok(instance.current_run_state.clone())
        } else {
            /* We're already processing a reboot.  No action needed. */
            assert_eq!(*current_run_state, ApiInstanceState::Stopping);
            assert_eq!(
                target_run_state.unwrap().run_state,
                ApiInstanceState::Stopped
            );
            assert!(instance.reboot_in_progress);
            info!(instance.log, "reboot: already in progress");
            Ok(instance.current_run_state.clone())
        };

        instances.insert(id.clone(), instance);
        rv
    }

    /**
     * Body of the background task (one per `SimInstance`) that simulates
     * Instance booting and halting.  Each time we read a message from the
     * instance's channel, we sleep for a bit and then invoke `instance_poke()`
     * to complete whatever transition is currently outstanding.
     */
    async fn instance_sim(&self, id: Uuid, mut rx: Receiver<()>) {
        while let Some(_) = rx.next().await {
            tokio::time::delay_for(Duration::from_millis(1500)).await;
            self.instance_poke(id).await;
        }
    }

    /**
     * Invoked as part of simulation to complete whatever asynchronous
     * transition is currently going on for instance `id`.
     */
    async fn instance_poke(&self, id: Uuid) {
        let (new_state, to_destroy) = {
            /* Do as little as possible with the lock held. */
            let mut instances = self.instances.lock().await;
            let mut instance = instances.remove(&id).unwrap();
            instance.transition_finish();
            let after = instance.current_run_state.clone();
            if instance.requested_run_state.is_none()
                && after.run_state == ApiInstanceState::Destroyed
            {
                info!(instance.log, "instance came to rest destroyed");
                (after, Some(instance))
            } else {
                instances.insert(id.clone(), instance);
                (after, None)
            }
        };

        /*
         * Notify the controller that the instance state has changed.  The
         * server controller is authoritative for the runtime state, and we use
         * a generation number here so that calls processed out of order do not
         * settle on the wrong value.
         * TODO-robustness: If this fails, we need to put it on some list of
         * updates to retry later.
         */
        self.ctlsc.notify_instance_updated(&id, &new_state).await.unwrap();

        /*
         * If the instance came to rest destroyed, complete any async cleanup
         * needed now.
         * TODO-debug It would be nice to have visibility into instances that
         * are cleaning up in case we have to debug resource leaks here.
         * TODO-correctness Is it a problem that nobody waits on the background
         * task?  If we did it here, we'd deadlock, since we're invoked from the
         * background task.
         */
        if let Some(destroyed_instance) = to_destroy {
            if let Some(mut tx) = destroyed_instance.channel_tx {
                tx.close_channel();
            }
        }
    }
}

/**
 * Trait used to expose interfaces for use only by the test suite.
 */
#[async_trait]
pub trait ServerControllerTestInterfaces {
    async fn instance_finish_transition(&self, id: Uuid);
}

#[async_trait]
impl ServerControllerTestInterfaces for ServerController {
    async fn instance_finish_transition(&self, id: Uuid) {
        self.instance_poke(id).await
    }
}

/**
 * `SimInstance` simulates an Oxide Rack Instance (virtual machine), as created
 * by the public API.
 *
 * We only simulate the Instance from the perspective of an API consumer, which
 * means for example accepting a request to boot it, reporting the current state
 * as "starting", and then some time later reporting that the state is
 * "running".
 */
struct SimInstance {
    /** Current runtime state of the instance */
    current_run_state: ApiInstanceRuntimeState,
    /**
     * Requested runtime state of the instance.  This field is non-None if and
     * only if we're currently simulating an asynchronous transition (e.g., boot
     * or halt).
     */
    requested_run_state: Option<ApiInstanceRuntimeStateParams>,

    /** Debug log */
    log: Logger,
    /** Channel for transmitting to the background task */
    channel_tx: Option<Sender<()>>,
    /**
     * True if a reboot is currently in progress.  This is set when we
     * transition to "Stopping" as part of a reboot and it's cleared when we
     * transition back to "Starting".
     */
    reboot_in_progress: bool,
}

/**
 * Buffer size for channel used to communicate with each SimInstance's
 * background task.  Messages sent on this channel trigger the task to simulate
 * an Instance state transition by sleeping for some interval and then updating
 * the Instance state.  When the background task updates the Instance state
 * after sleeping, it always looks at the current state to decide what to do.
 * As a result, we never need to queue up more than one transition.  In turn,
 * that means we don't need (or want) a channel buffer larger than 1.  If we
 * were to queue up multiple messages in the buffer, the net effect would be
 * exactly the same as if just one message were queued.  (Because of what we
 * said above, as part of processing that message, the receiver will wind up
 * handling all state transitions requested up to the point where the first
 * message is read.  If another transition is requested after that point,
 * another message will be enqueued and the receiver will process that
 * transition then.  There's no need to queue more than one message.)  Even
 * stronger: we don't want a larger buffer because that would only cause extra
 * laps through the sleep cycle, which just wastes resources and increases the
 * latency for processing the next real transition request.
 */
const SIM_INSTANCE_CHANNEL_BUFFER_SIZE: usize = 0;

impl SimInstance {
    /**
     * Create a new `SimInstance` with state transitions automatically
     * simulated by a background task.  The caller is expected to provide the
     * background task that reads from the channel and advances the simulation.
     */
    fn new_simulated_auto(
        initial_runtime: &ApiInstanceRuntimeState,
        log: Logger,
    ) -> (SimInstance, Receiver<()>) {
        debug!(log, "created simulated instance";
            "initial_state" => ?initial_runtime);
        let (tx, rx) =
            futures::channel::mpsc::channel(SIM_INSTANCE_CHANNEL_BUFFER_SIZE);
        (
            SimInstance {
                current_run_state: initial_runtime.clone(),
                requested_run_state: None,
                reboot_in_progress: false,
                log,
                channel_tx: Some(tx),
            },
            rx,
        )
    }

    /**
     * Create a new `SimInstance` with state transitions simulated by explicit
     * calls.  The only difference from the perspective of this struct is that
     * we won't have a channel to which we send notifications when asynchronous
     * state transitions begin.
     */
    fn new_simulated_explicit(
        initial_runtime: &ApiInstanceRuntimeState,
        log: Logger,
    ) -> SimInstance {
        debug!(log, "created simulated instance";
            "initial_state" => ?initial_runtime);
        SimInstance {
            current_run_state: initial_runtime.clone(),
            requested_run_state: None,
            reboot_in_progress: false,
            log,
            channel_tx: None,
        }
    }

    /**
     * Transition this Instance to state `target`.  In some cases, the
     * transition may happen immediately (e.g., going from "Stopped" to
     * "Destroyed").  In other cases, as when going from "Stopped" to "Running",
     * we immediately transition to an intermediate state ("Starting", in this
     * case), simulate the transition, and some time later update to the desired
     * state.
     *
     * This function supports transitions that don't change the state at all
     * (either because the requested state change was `None` -- maybe we were
     * changing some other runtime state parameter -- or because we're already
     * in the desired state).
     */
    fn transition(
        &mut self,
        target: &ApiInstanceRuntimeStateParams,
    ) -> Option<ApiInstanceRuntimeStateParams> {
        /*
         * In all cases, set `requested_run_state` to the new target.  If there
         * was already a requested run state, we will return this to the caller
         * so that they can log a possible dropped transition.  This is only
         * intended for debugging.
         */
        let dropped = self.requested_run_state.take();

        /*
         * Any requested transition causes us to abandon a reboot in progress.
         */
        self.reboot_in_progress = false;

        let state_before = self.current_run_state.run_state.clone();
        let state_after = match target.run_state {
            ref target_run_state if *target_run_state == state_before => {
                debug!(self.log, "noop transition";
                    "target" => ?target);
                return dropped;
            }

            /*
             * For intermediate states (which don't really make sense to
             * request), just try to do the closest reasonable thing.
             * TODO-cleanup Use a different type here.
             */
            ApiInstanceState::Creating => &ApiInstanceState::Running,
            ApiInstanceState::Starting => &ApiInstanceState::Running,
            ApiInstanceState::Stopping => &ApiInstanceState::Stopped,

            /* This is the most common interesting case. */
            ref target_run_state => target_run_state,
        };

        /*
         * Depending on what state we're in and what state we're going to, we
         * may need to transition to an intermediate state before we can get to
         * the requested state.  In that case, we'll asynchronously simulate the
         * transition.
         */
        let (immed_next_state, need_async) =
            if state_before.is_stopped() && !state_after.is_stopped() {
                (&ApiInstanceState::Starting, true)
            } else if !state_before.is_stopped() && state_after.is_stopped() {
                (&ApiInstanceState::Stopping, true)
            } else {
                (state_after, false)
            };

        /*
         * Update the current state to reflect what we've decided -- either
         * going directly to the requested state or to an intermediate state.
         */
        self.current_run_state = ApiInstanceRuntimeState {
            run_state: immed_next_state.clone(),
            server_uuid: self.current_run_state.server_uuid.clone(),
            gen: self.current_run_state.gen + 1,
            time_updated: Utc::now(),
        };

        debug!(self.log, "instance transition";
            "state_before" => %state_before,
            "state_after" => %state_after,
            "immed_next_state" => %immed_next_state,
            "dropped" => ?dropped,
            "async" => %need_async,
            "new_runtime" => ?self.current_run_state
        );

        /*
         * If this is an asynchronous transition, notify the background task to
         * simulate it.  There are a few possible error cases:
         *
         * (1) We fail to send the message because the channel's buffer is full.
         *     All we need to guarantee in the first place is that the receiver
         *     will receive a message at least once after this function is
         *     invoked.  If there's already a message in the buffer, we don't
         *     need to do anything else to achieve that.
         *
         * (2) We fail to send the message because the channel is disconnected.
         *     This would be a programmer error -- the contract between us and
         *     the receiver is that we shut down the channel first.  As a
         *     result, we panic if we find this case.
         *
         * (3) We failed to send the message for some other reason.  This
         *     appears impossible at the time of this writing.   It would be
         *     nice if the returned error type were implemented in a way that we
         *     could identify this case at compile time (e.g., using an enum),
         *     but that's not currently the case.
         */
        if need_async {
            /*
             * XXX target.clone() is wrong if we were invoked with Stopping or
             * Starting -- it should be rewritten to use state_after
             */
            self.requested_run_state = Some(target.clone());
            if let Some(ref mut tx) = self.channel_tx {
                let result = tx.try_send(());
                if let Err(error) = result {
                    assert!(!error.is_disconnected());
                    assert!(error.is_full());
                }
            }
        }

        dropped
    }

    /**
     * Finish simulating a "boot" or "halt" transition.
     */
    fn transition_finish(&mut self) {
        let requested_run_state = match self.requested_run_state.take() {
            /*
             * Somebody must have requested a state change while we were
             * simulating a previous asynchronous one.  By definition, the new
             * one must also be asynchronous, and the first of the two calls to
             * `transition_finish()` will complete the new transition.  The
             * second one will find us here.
             * TODO-cleanup We could probably eliminate this case by not
             * sending a message to the background task if we were already in an
             * async transition.
             */
            None => {
                debug!(self.log, "noop transition finish";
                    "current_run_state" => %self.current_run_state.run_state);
                return;
            }
            Some(run_state) => run_state,
        };

        /*
         * As documented above, `self.requested_run_state` is only non-None when
         * there's an asynchronous (simulated) transition in progress, and the
         * only such transitions start at "Starting" or "Stopping" and go to
         * "Running" or one of several stopped states, respectively.  Since we
         * checked `self.requested_run_state` above, we know we're in one of
         * these two transitions and assert that here.
         */
        let run_state_before = self.current_run_state.run_state.clone();
        let run_state_after = requested_run_state.run_state;
        match run_state_before {
            ApiInstanceState::Starting => {
                assert_eq!(run_state_after, ApiInstanceState::Running);
                assert!(!self.reboot_in_progress);
            }
            ApiInstanceState::Stopping => {
                assert!(run_state_after.is_stopped());
            }
            _ => panic!("async transition started for unexpected state"),
        };

        /*
         * Having verified all that, we can update the Instance's state.
         */
        self.current_run_state = ApiInstanceRuntimeState {
            run_state: run_state_after.clone(),
            server_uuid: self.current_run_state.server_uuid.clone(),
            gen: self.current_run_state.gen + 1,
            time_updated: Utc::now(),
        };

        debug!(self.log, "simulated transition finish";
            "state_before" => %run_state_before,
            "state_after" => %run_state_after,
            "new_runtime" => ?self.current_run_state
        );

        if self.reboot_in_progress {
            /*
             * XXX we need to notify the controller about this intermediate
             * state change but we don't have a handle to the SC in order to do
             * that.
             */
            assert_eq!(
                self.current_run_state.run_state,
                ApiInstanceState::Stopped
            );
            self.reboot_in_progress = false;
            self.transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
            });
        }
    }
}

#[cfg(test)]
mod test {
    use super::SimInstance;
    use crate::api_model::ApiInstanceRuntimeState;
    use crate::api_model::ApiInstanceRuntimeStateParams;
    use crate::api_model::ApiInstanceState;
    use crate::test_util::test_setup_log;
    use chrono::Utc;
    use dropshot::test_util::LogContext;
    use futures::channel::mpsc::Receiver;

    fn make_instance(
        logctx: &LogContext,
        initial_state: ApiInstanceState,
    ) -> (SimInstance, Receiver<()>) {
        let now = Utc::now();
        let initial_runtime = {
            ApiInstanceRuntimeState {
                run_state: initial_state,
                server_uuid: uuid::Uuid::new_v4(),
                gen: 1,
                time_updated: now,
            }
        };

        SimInstance::new_simulated_auto(&initial_runtime, logctx.log.new(o!()))
    }

    #[tokio::test]
    async fn test_sim_instance() {
        let logctx = test_setup_log("test_sim_instance").await;
        let (mut instance, mut rx) =
            make_instance(&logctx, ApiInstanceState::Creating);
        let r1 = instance.current_run_state.clone();

        info!(logctx.log, "new instance"; "run_state" => ?r1.run_state);
        assert_eq!(r1.run_state, ApiInstanceState::Creating);
        assert_eq!(r1.gen, 1);

        /*
         * There's no asynchronous transition going on yet so a
         * transition_finish() shouldn't change anything.
         */
        assert!(instance.requested_run_state.is_none());
        instance.transition_finish();
        assert!(instance.requested_run_state.is_none());
        assert_eq!(&r1.time_updated, &instance.current_run_state.time_updated);
        assert_eq!(&r1.run_state, &instance.current_run_state.run_state);
        assert_eq!(r1.gen, instance.current_run_state.gen);
        assert!(rx.try_next().is_err());

        /*
         * We should be able to transition immediately to any other stopped
         * state, including the one we're already in ("Creating")
         */
        let stopped_states = vec![
            ApiInstanceState::Creating,
            ApiInstanceState::Stopped,
            ApiInstanceState::Repairing,
            ApiInstanceState::Failed,
            ApiInstanceState::Destroyed,
        ];
        let mut rprev = r1;
        for state in stopped_states {
            assert!(rprev.run_state.is_stopped());
            let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
                run_state: state.clone(),
            });
            assert!(dropped.is_none());
            assert!(instance.requested_run_state.is_none());
            let rnext = instance.current_run_state.clone();
            if state != rprev.run_state {
                assert!(rnext.gen > rprev.gen);
            }
            assert!(rnext.time_updated >= rprev.time_updated);
            assert_eq!(rnext.run_state, state);
            assert!(rx.try_next().is_err());
            rprev = rnext;
        }

        /*
         * Now, if we transition to "Running", we must go through the async
         * process.
         */
        assert!(rprev.run_state.is_stopped());
        assert!(rx.try_next().is_err());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Running,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_some());
        assert!(rx.try_next().is_ok());
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        assert!(!rnext.run_state.is_stopped());
        rprev = rnext;

        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert!(instance.requested_run_state.is_none());
        assert!(rx.try_next().is_err());
        assert_eq!(rprev.run_state, ApiInstanceState::Starting);
        assert_eq!(rnext.run_state, ApiInstanceState::Running);
        rprev = rnext;
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rprev.gen, rnext.gen);

        /*
         * If we transition again to "Running", the process should complete
         * immediately.
         */
        assert!(!rprev.run_state.is_stopped());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Running,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_none());
        assert!(rx.try_next().is_err());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.gen, rprev.gen);
        assert_eq!(rnext.time_updated, rprev.time_updated);
        assert_eq!(rnext.run_state, rprev.run_state);
        rprev = rnext;

        /*
         * If we go back to any stopped state, we go through the async process
         * again.
         */
        assert!(!rprev.run_state.is_stopped());
        assert!(rx.try_next().is_err());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Destroyed,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_some());
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        assert!(!rnext.run_state.is_stopped());
        rprev = rnext;

        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert!(instance.requested_run_state.is_none());
        assert_eq!(rprev.run_state, ApiInstanceState::Stopping);
        assert_eq!(rnext.run_state, ApiInstanceState::Destroyed);
        rprev = rnext;
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rprev.gen, rnext.gen);

        /*
         * Now let's test the behavior of dropping a transition.  We'll start
         * transitioning back to "Running".  Then, while we're still in
         * "Starting", will transition back to "Destroyed".  We should
         * immediately go to "Stopping", and completing the transition should
         * take us to "Destroyed".
         */
        assert!(rprev.run_state.is_stopped());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Running,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_some());
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        assert!(!rnext.run_state.is_stopped());
        rprev = rnext;

        /*
         * Interrupt the async transition with a new one.
         */
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Destroyed,
        });
        assert_eq!(dropped.unwrap().run_state, ApiInstanceState::Running);
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        rprev = rnext;

        /*
         * Finish the async transition.
         */
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert!(instance.requested_run_state.is_none());
        assert_eq!(rprev.run_state, ApiInstanceState::Stopping);
        assert_eq!(rnext.run_state, ApiInstanceState::Destroyed);
        rprev = rnext;
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rprev.gen, rnext.gen);

        logctx.cleanup_successful();
    }
}

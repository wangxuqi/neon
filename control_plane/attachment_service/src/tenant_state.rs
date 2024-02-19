use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::metrics;
use control_plane::attachment_service::NodeAvailability;
use pageserver_api::{
    models::{LocationConfig, LocationConfigMode, TenantConfig},
    shard::{ShardIdentity, TenantShardId},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{instrument, Instrument};
use utils::{
    generation::Generation,
    id::NodeId,
    seqwait::{SeqWait, SeqWaitError},
    sync::gate::Gate,
};

use crate::{
    compute_hook::ComputeHook,
    node::Node,
    persistence::{split_state::SplitState, Persistence},
    reconciler::{
        attached_location_conf, secondary_location_conf, ReconcileError, Reconciler, TargetState,
    },
    scheduler::{ScheduleError, Scheduler},
    service, PlacementPolicy, Sequence,
};

pub(crate) struct TenantState {
    pub(crate) tenant_shard_id: TenantShardId,

    pub(crate) shard: ShardIdentity,

    // Runtime only: sequence used to coordinate when updating this object while
    // with background reconcilers may be running.  A reconciler runs to a particular
    // sequence.
    pub(crate) sequence: Sequence,

    // Latest generation number: next time we attach, increment this
    // and use the incremented number when attaching
    pub(crate) generation: Generation,

    // High level description of how the tenant should be set up.  Provided
    // externally.
    pub(crate) policy: PlacementPolicy,

    // Low level description of exactly which pageservers should fulfil
    // which role.  Generated by `Self::schedule`.
    pub(crate) intent: IntentState,

    // Low level description of how the tenant is configured on pageservers:
    // if this does not match `Self::intent` then the tenant needs reconciliation
    // with `Self::reconcile`.
    pub(crate) observed: ObservedState,

    // Tenant configuration, passed through opaquely to the pageserver.  Identical
    // for all shards in a tenant.
    pub(crate) config: TenantConfig,

    /// If a reconcile task is currently in flight, it may be joined here (it is
    /// only safe to join if either the result has been received or the reconciler's
    /// cancellation token has been fired)
    pub(crate) reconciler: Option<ReconcilerHandle>,

    /// If a tenant is being split, then all shards with that TenantId will have a
    /// SplitState set, this acts as a guard against other operations such as background
    /// reconciliation, and timeline creation.
    pub(crate) splitting: SplitState,

    /// Optionally wait for reconciliation to complete up to a particular
    /// sequence number.
    pub(crate) waiter: std::sync::Arc<SeqWait<Sequence, Sequence>>,

    /// Indicates sequence number for which we have encountered an error reconciling.  If
    /// this advances ahead of [`Self::waiter`] then a reconciliation error has occurred,
    /// and callers should stop waiting for `waiter` and propagate the error.
    pub(crate) error_waiter: std::sync::Arc<SeqWait<Sequence, Sequence>>,

    /// The most recent error from a reconcile on this tenant
    /// TODO: generalize to an array of recent events
    /// TOOD: use a ArcSwap instead of mutex for faster reads?
    pub(crate) last_error: std::sync::Arc<std::sync::Mutex<String>>,

    /// If we have a pending compute notification that for some reason we weren't able to send,
    /// set this to true. If this is set, calls to [`Self::maybe_reconcile`] will run a task to retry
    /// sending it.  This is the mechanism by which compute notifications are included in the scope
    /// of state that we publish externally in an eventually consistent way.
    pub(crate) pending_compute_notification: bool,
}

#[derive(Default, Clone, Debug)]
pub(crate) struct IntentState {
    attached: Option<NodeId>,
    secondary: Vec<NodeId>,
}

impl IntentState {
    pub(crate) fn new() -> Self {
        Self {
            attached: None,
            secondary: vec![],
        }
    }
    pub(crate) fn single(scheduler: &mut Scheduler, node_id: Option<NodeId>) -> Self {
        if let Some(node_id) = node_id {
            scheduler.node_inc_ref(node_id);
        }
        Self {
            attached: node_id,
            secondary: vec![],
        }
    }

    pub(crate) fn set_attached(&mut self, scheduler: &mut Scheduler, new_attached: Option<NodeId>) {
        if self.attached != new_attached {
            if let Some(old_attached) = self.attached.take() {
                scheduler.node_dec_ref(old_attached);
            }
            if let Some(new_attached) = &new_attached {
                scheduler.node_inc_ref(*new_attached);
            }
            self.attached = new_attached;
        }
    }

    pub(crate) fn push_secondary(&mut self, scheduler: &mut Scheduler, new_secondary: NodeId) {
        debug_assert!(!self.secondary.contains(&new_secondary));
        scheduler.node_inc_ref(new_secondary);
        self.secondary.push(new_secondary);
    }

    /// It is legal to call this with a node that is not currently a secondary: that is a no-op
    pub(crate) fn remove_secondary(&mut self, scheduler: &mut Scheduler, node_id: NodeId) {
        let index = self.secondary.iter().position(|n| *n == node_id);
        if let Some(index) = index {
            scheduler.node_dec_ref(node_id);
            self.secondary.remove(index);
        }
    }

    pub(crate) fn clear_secondary(&mut self, scheduler: &mut Scheduler) {
        for secondary in self.secondary.drain(..) {
            scheduler.node_dec_ref(secondary);
        }
    }

    pub(crate) fn clear(&mut self, scheduler: &mut Scheduler) {
        if let Some(old_attached) = self.attached.take() {
            scheduler.node_dec_ref(old_attached);
        }

        self.clear_secondary(scheduler);
    }

    pub(crate) fn all_pageservers(&self) -> Vec<NodeId> {
        let mut result = Vec::new();
        if let Some(p) = self.attached {
            result.push(p)
        }

        result.extend(self.secondary.iter().copied());

        result
    }

    pub(crate) fn get_attached(&self) -> &Option<NodeId> {
        &self.attached
    }

    pub(crate) fn get_secondary(&self) -> &Vec<NodeId> {
        &self.secondary
    }

    /// When a node goes offline, we update intents to avoid using it
    /// as their attached pageserver.
    ///
    /// Returns true if a change was made
    pub(crate) fn notify_offline(&mut self, node_id: NodeId) -> bool {
        if self.attached == Some(node_id) {
            self.attached = None;
            self.secondary.push(node_id);
            true
        } else {
            false
        }
    }
}

impl Drop for IntentState {
    fn drop(&mut self) {
        // Must clear before dropping, to avoid leaving stale refcounts in the Scheduler
        debug_assert!(self.attached.is_none() && self.secondary.is_empty());
    }
}

#[derive(Default, Clone)]
pub(crate) struct ObservedState {
    pub(crate) locations: HashMap<NodeId, ObservedStateLocation>,
}

/// Our latest knowledge of how this tenant is configured in the outside world.
///
/// Meaning:
///     * No instance of this type exists for a node: we are certain that we have nothing configured on that
///       node for this shard.
///     * Instance exists with conf==None: we *might* have some state on that node, but we don't know
///       what it is (e.g. we failed partway through configuring it)
///     * Instance exists with conf==Some: this tells us what we last successfully configured on this node,
///       and that configuration will still be present unless something external interfered.
#[derive(Clone)]
pub(crate) struct ObservedStateLocation {
    /// If None, it means we do not know the status of this shard's location on this node, but
    /// we know that we might have some state on this node.
    pub(crate) conf: Option<LocationConfig>,
}
pub(crate) struct ReconcilerWaiter {
    // For observability purposes, remember the ID of the shard we're
    // waiting for.
    pub(crate) tenant_shard_id: TenantShardId,

    seq_wait: std::sync::Arc<SeqWait<Sequence, Sequence>>,
    error_seq_wait: std::sync::Arc<SeqWait<Sequence, Sequence>>,
    error: std::sync::Arc<std::sync::Mutex<String>>,
    seq: Sequence,
}

#[derive(thiserror::Error, Debug)]
pub enum ReconcileWaitError {
    #[error("Timeout waiting for shard {0}")]
    Timeout(TenantShardId),
    #[error("shutting down")]
    Shutdown,
    #[error("Reconcile error on shard {0}: {1}")]
    Failed(TenantShardId, String),
}

impl ReconcilerWaiter {
    pub(crate) async fn wait_timeout(&self, timeout: Duration) -> Result<(), ReconcileWaitError> {
        tokio::select! {
            result = self.seq_wait.wait_for_timeout(self.seq, timeout)=> {
                result.map_err(|e| match e {
                    SeqWaitError::Timeout => ReconcileWaitError::Timeout(self.tenant_shard_id),
                    SeqWaitError::Shutdown => ReconcileWaitError::Shutdown
                })?;
            },
            result = self.error_seq_wait.wait_for(self.seq) => {
                result.map_err(|e| match e {
                    SeqWaitError::Shutdown => ReconcileWaitError::Shutdown,
                    SeqWaitError::Timeout => unreachable!()
                })?;

                return Err(ReconcileWaitError::Failed(self.tenant_shard_id, self.error.lock().unwrap().clone()))
            }
        }

        Ok(())
    }
}

/// Having spawned a reconciler task, the tenant shard's state will carry enough
/// information to optionally cancel & await it later.
pub(crate) struct ReconcilerHandle {
    sequence: Sequence,
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

/// When a reconcile task completes, it sends this result object
/// to be applied to the primary TenantState.
pub(crate) struct ReconcileResult {
    pub(crate) sequence: Sequence,
    /// On errors, `observed` should be treated as an incompleted description
    /// of state (i.e. any nodes present in the result should override nodes
    /// present in the parent tenant state, but any unmentioned nodes should
    /// not be removed from parent tenant state)
    pub(crate) result: Result<(), ReconcileError>,

    pub(crate) tenant_shard_id: TenantShardId,
    pub(crate) generation: Generation,
    pub(crate) observed: ObservedState,

    /// Set [`TenantState::pending_compute_notification`] from this flag
    pub(crate) pending_compute_notification: bool,
}

impl ObservedState {
    pub(crate) fn new() -> Self {
        Self {
            locations: HashMap::new(),
        }
    }
}

impl TenantState {
    pub(crate) fn new(
        tenant_shard_id: TenantShardId,
        shard: ShardIdentity,
        policy: PlacementPolicy,
    ) -> Self {
        Self {
            tenant_shard_id,
            policy,
            intent: IntentState::default(),
            generation: Generation::new(0),
            shard,
            observed: ObservedState::default(),
            config: TenantConfig::default(),
            reconciler: None,
            splitting: SplitState::Idle,
            sequence: Sequence(1),
            waiter: Arc::new(SeqWait::new(Sequence(0))),
            error_waiter: Arc::new(SeqWait::new(Sequence(0))),
            last_error: Arc::default(),
            pending_compute_notification: false,
        }
    }

    /// For use on startup when learning state from pageservers: generate my [`IntentState`] from my
    /// [`ObservedState`], even if it violates my [`PlacementPolicy`].  Call [`Self::schedule`] next,
    /// to get an intent state that complies with placement policy.  The overall goal is to do scheduling
    /// in a way that makes use of any configured locations that already exist in the outside world.
    pub(crate) fn intent_from_observed(&mut self) {
        // Choose an attached location by filtering observed locations, and then sorting to get the highest
        // generation
        let mut attached_locs = self
            .observed
            .locations
            .iter()
            .filter_map(|(node_id, l)| {
                if let Some(conf) = &l.conf {
                    if conf.mode == LocationConfigMode::AttachedMulti
                        || conf.mode == LocationConfigMode::AttachedSingle
                        || conf.mode == LocationConfigMode::AttachedStale
                    {
                        Some((node_id, conf.generation))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        attached_locs.sort_by_key(|i| i.1);
        if let Some((node_id, _gen)) = attached_locs.into_iter().last() {
            self.intent.attached = Some(*node_id);
        }

        // All remaining observed locations generate secondary intents.  This includes None
        // observations, as these may well have some local content on disk that is usable (this
        // is an edge case that might occur if we restarted during a migration or other change)
        self.observed.locations.keys().for_each(|node_id| {
            if Some(*node_id) != self.intent.attached {
                self.intent.secondary.push(*node_id);
            }
        });
    }

    pub(crate) fn schedule(&mut self, scheduler: &mut Scheduler) -> Result<(), ScheduleError> {
        // TODO: before scheduling new nodes, check if any existing content in
        // self.intent refers to pageservers that are offline, and pick other
        // pageservers if so.

        // TODO: respect the splitting bit on tenants: if they are currently splitting then we may not
        // change their attach location.

        // Build the set of pageservers already in use by this tenant, to avoid scheduling
        // more work on the same pageservers we're already using.
        let mut used_pageservers = self.intent.all_pageservers();
        let mut modified = false;

        use PlacementPolicy::*;
        match self.policy {
            Single => {
                // Should have exactly one attached, and zero secondaries
                if self.intent.attached.is_none() {
                    let node_id = scheduler.schedule_shard(&used_pageservers)?;
                    self.intent.set_attached(scheduler, Some(node_id));
                    used_pageservers.push(node_id);
                    modified = true;
                }
                if !self.intent.secondary.is_empty() {
                    self.intent.clear_secondary(scheduler);
                    modified = true;
                }
            }
            Double(secondary_count) => {
                // Should have exactly one attached, and N secondaries
                if self.intent.attached.is_none() {
                    let node_id = scheduler.schedule_shard(&used_pageservers)?;
                    self.intent.set_attached(scheduler, Some(node_id));
                    used_pageservers.push(node_id);
                    modified = true;
                }

                while self.intent.secondary.len() < secondary_count {
                    let node_id = scheduler.schedule_shard(&used_pageservers)?;
                    self.intent.push_secondary(scheduler, node_id);
                    used_pageservers.push(node_id);
                    modified = true;
                }
            }
            Detached => {
                // Should have no attached or secondary pageservers
                if self.intent.attached.is_some() {
                    self.intent.set_attached(scheduler, None);
                    modified = true;
                }

                if !self.intent.secondary.is_empty() {
                    self.intent.clear_secondary(scheduler);
                    modified = true;
                }
            }
        }

        if modified {
            self.sequence.0 += 1;
        }

        Ok(())
    }

    /// Query whether the tenant's observed state for attached node matches its intent state, and if so,
    /// yield the node ID.  This is appropriate for emitting compute hook notifications: we are checking that
    /// the node in question is not only where we intend to attach, but that the tenant is indeed already attached there.
    ///
    /// Reconciliation may still be needed for other aspects of state such as secondaries (see [`Self::dirty`]): this
    /// funciton should not be used to decide whether to reconcile.
    pub(crate) fn stably_attached(&self) -> Option<NodeId> {
        if let Some(attach_intent) = self.intent.attached {
            match self.observed.locations.get(&attach_intent) {
                Some(loc) => match &loc.conf {
                    Some(conf) => match conf.mode {
                        LocationConfigMode::AttachedMulti
                        | LocationConfigMode::AttachedSingle
                        | LocationConfigMode::AttachedStale => {
                            // Our intent and observed state agree that this node is in an attached state.
                            Some(attach_intent)
                        }
                        // Our observed config is not an attached state
                        _ => None,
                    },
                    // Our observed state is None, i.e. in flux
                    None => None,
                },
                // We have no observed state for this node
                None => None,
            }
        } else {
            // Our intent is not to attach
            None
        }
    }

    fn dirty(&self) -> bool {
        if let Some(node_id) = self.intent.attached {
            let wanted_conf = attached_location_conf(self.generation, &self.shard, &self.config);
            match self.observed.locations.get(&node_id) {
                Some(conf) if conf.conf.as_ref() == Some(&wanted_conf) => {}
                Some(_) | None => {
                    return true;
                }
            }
        }

        for node_id in &self.intent.secondary {
            let wanted_conf = secondary_location_conf(&self.shard, &self.config);
            match self.observed.locations.get(node_id) {
                Some(conf) if conf.conf.as_ref() == Some(&wanted_conf) => {}
                Some(_) | None => {
                    return true;
                }
            }
        }

        // Even if there is no pageserver work to be done, if we have a pending notification to computes,
        // wake up a reconciler to send it.
        if self.pending_compute_notification {
            return true;
        }

        false
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(tenant_id=%self.tenant_shard_id.tenant_id, shard_id=%self.tenant_shard_id.shard_slug()))]
    pub(crate) fn maybe_reconcile(
        &mut self,
        result_tx: tokio::sync::mpsc::UnboundedSender<ReconcileResult>,
        pageservers: &Arc<HashMap<NodeId, Node>>,
        compute_hook: &Arc<ComputeHook>,
        service_config: &service::Config,
        persistence: &Arc<Persistence>,
        gate: &Gate,
        cancel: &CancellationToken,
    ) -> Option<ReconcilerWaiter> {
        // If there are any ambiguous observed states, and the nodes they refer to are available,
        // we should reconcile to clean them up.
        let mut dirty_observed = false;
        for (node_id, observed_loc) in &self.observed.locations {
            let node = pageservers
                .get(node_id)
                .expect("Nodes may not be removed while referenced");
            if observed_loc.conf.is_none()
                && !matches!(node.availability, NodeAvailability::Offline)
            {
                dirty_observed = true;
                break;
            }
        }

        if !self.dirty() && !dirty_observed {
            tracing::info!("Not dirty, no reconciliation needed.");
            return None;
        }

        // If we are currently splitting, then never start a reconciler task: the splitting logic
        // requires that shards are not interfered with while it runs. Do this check here rather than
        // up top, so that we only log this message if we would otherwise have done a reconciliation.
        if !matches!(self.splitting, SplitState::Idle) {
            tracing::info!("Refusing to reconcile, splitting in progress");
            return None;
        }

        // Reconcile already in flight for the current sequence?
        if let Some(handle) = &self.reconciler {
            if handle.sequence == self.sequence {
                return Some(ReconcilerWaiter {
                    tenant_shard_id: self.tenant_shard_id,
                    seq_wait: self.waiter.clone(),
                    error_seq_wait: self.error_waiter.clone(),
                    error: self.last_error.clone(),
                    seq: self.sequence,
                });
            }
        }

        // Reconcile in flight for a stale sequence?  Our sequence's task will wait for it before
        // doing our sequence's work.
        let old_handle = self.reconciler.take();

        let Ok(gate_guard) = gate.enter() else {
            // Shutting down, don't start a reconciler
            return None;
        };

        let reconciler_cancel = cancel.child_token();
        let mut reconciler = Reconciler {
            tenant_shard_id: self.tenant_shard_id,
            shard: self.shard,
            generation: self.generation,
            intent: TargetState::from_intent(&self.intent),
            config: self.config.clone(),
            observed: self.observed.clone(),
            pageservers: pageservers.clone(),
            compute_hook: compute_hook.clone(),
            service_config: service_config.clone(),
            _gate_guard: gate_guard,
            cancel: reconciler_cancel.clone(),
            persistence: persistence.clone(),
            compute_notify_failure: false,
        };

        let reconcile_seq = self.sequence;

        tracing::info!(seq=%reconcile_seq, "Spawning Reconciler for sequence {}", self.sequence);
        let must_notify = self.pending_compute_notification;
        let reconciler_span = tracing::info_span!(parent: None, "reconciler", seq=%reconcile_seq,
                                                        tenant_id=%reconciler.tenant_shard_id.tenant_id,
                                                        shard_id=%reconciler.tenant_shard_id.shard_slug());
        metrics::RECONCILER.spawned.inc();
        let join_handle = tokio::task::spawn(
            async move {
                // Wait for any previous reconcile task to complete before we start
                if let Some(old_handle) = old_handle {
                    old_handle.cancel.cancel();
                    if let Err(e) = old_handle.handle.await {
                        // We can't do much with this other than log it: the task is done, so
                        // we may proceed with our work.
                        tracing::error!("Unexpected join error waiting for reconcile task: {e}");
                    }
                }

                // Early check for cancellation before doing any work
                // TODO: wrap all remote API operations in cancellation check
                // as well.
                if reconciler.cancel.is_cancelled() {
                    metrics::RECONCILER
                        .complete
                        .with_label_values(&[metrics::ReconcilerMetrics::CANCEL])
                        .inc();
                    return;
                }

                // Attempt to make observed state match intent state
                let result = reconciler.reconcile().await;

                // If we know we had a pending compute notification from some previous action, send a notification irrespective
                // of whether the above reconcile() did any work
                if result.is_ok() && must_notify {
                    // If this fails we will send the need to retry in [`ReconcileResult::pending_compute_notification`]
                    reconciler.compute_notify().await.ok();
                }

                // Update result counter
                match &result {
                    Ok(_) => metrics::RECONCILER
                        .complete
                        .with_label_values(&[metrics::ReconcilerMetrics::SUCCESS]),
                    Err(ReconcileError::Cancel) => metrics::RECONCILER
                        .complete
                        .with_label_values(&[metrics::ReconcilerMetrics::CANCEL]),
                    Err(_) => metrics::RECONCILER
                        .complete
                        .with_label_values(&[metrics::ReconcilerMetrics::ERROR]),
                }
                .inc();

                result_tx
                    .send(ReconcileResult {
                        sequence: reconcile_seq,
                        result,
                        tenant_shard_id: reconciler.tenant_shard_id,
                        generation: reconciler.generation,
                        observed: reconciler.observed,
                        pending_compute_notification: reconciler.compute_notify_failure,
                    })
                    .ok();
            }
            .instrument(reconciler_span),
        );

        self.reconciler = Some(ReconcilerHandle {
            sequence: self.sequence,
            handle: join_handle,
            cancel: reconciler_cancel,
        });

        Some(ReconcilerWaiter {
            tenant_shard_id: self.tenant_shard_id,
            seq_wait: self.waiter.clone(),
            error_seq_wait: self.error_waiter.clone(),
            error: self.last_error.clone(),
            seq: self.sequence,
        })
    }

    // If we had any state at all referring to this node ID, drop it.  Does not
    // attempt to reschedule.
    pub(crate) fn deref_node(&mut self, node_id: NodeId) {
        if self.intent.attached == Some(node_id) {
            self.intent.attached = None;
        }

        self.intent.secondary.retain(|n| n != &node_id);

        self.observed.locations.remove(&node_id);

        debug_assert!(!self.intent.all_pageservers().contains(&node_id));
    }
}

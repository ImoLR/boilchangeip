use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use teloxide::types::{ChatId, MessageId};
use tokio::sync::Mutex;

use crate::{
    config::{AppConfig, SecretToken},
    timer::TimerManager,
};

pub(super) const CONFIRM_TTL: Duration = Duration::from_secs(120);
pub(super) const TIMER_INPUT_TTL: Duration = Duration::from_secs(300);
pub(super) const SERVER_WIZARD_TTL: Duration = Duration::from_secs(900);
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(super) struct PendingConfirmation {
    server_id: String,
    expires_at: Instant,
    used: bool,
}

#[derive(Default)]
pub(super) struct ConfirmationStore {
    pending: HashMap<String, PendingConfirmation>,
}

#[derive(Clone)]
pub(super) enum TimerInputMode {
    New,
    Edit(crate::timer::TimerTarget),
}

#[derive(Clone)]
pub(super) struct PendingTimerInput {
    mode: TimerInputMode,
    expires_at: Instant,
}

#[derive(Default)]
pub(super) struct TimerInputStore {
    pending: HashMap<ChatId, PendingTimerInput>,
}

#[derive(Default)]
pub(super) struct TimerMessageStore {
    messages: HashMap<ChatId, Vec<MessageId>>,
}

#[derive(Clone, Debug)]
pub(super) enum ServerWizardStep {
    Token,
    Name {
        current_ip: String,
        token: SecretToken,
    },
}

#[derive(Clone, Debug)]
pub(super) struct PendingServerWizard {
    step: ServerWizardStep,
    expires_at: Instant,
}

#[derive(Default)]
pub(super) struct ServerWizardStore {
    pending: HashMap<ChatId, PendingServerWizard>,
}

#[derive(Clone, Debug)]
pub(super) enum ServerEditMode {
    Name { server_id: String },
    Address { server_id: String },
    Token { server_id: String },
}

#[derive(Clone, Debug)]
pub(super) struct PendingServerEdit {
    mode: ServerEditMode,
    expires_at: Instant,
}

#[derive(Default)]
pub(super) struct ServerEditStore {
    pending: HashMap<ChatId, PendingServerEdit>,
}

#[derive(Default)]
pub(super) struct BotBusyStore {
    busy_chats: StdMutex<HashSet<ChatId>>,
}

pub(super) struct BotBusyGuard {
    chat_id: ChatId,
    busy_chats: Arc<BotBusyStore>,
}

#[derive(Clone)]
pub(super) struct BotShared {
    pub(super) config: Arc<Mutex<AppConfig>>,
    pub(super) timer: Arc<Mutex<TimerManager>>,
    pub(super) confirmations: Arc<Mutex<ConfirmationStore>>,
    pub(super) timer_inputs: Arc<Mutex<TimerInputStore>>,
    pub(super) timer_messages: Arc<Mutex<TimerMessageStore>>,
    pub(super) server_wizards: Arc<Mutex<ServerWizardStore>>,
    pub(super) server_edits: Arc<Mutex<ServerEditStore>>,
    pub(super) busy: Arc<BotBusyStore>,
}

impl BotShared {
    pub(super) fn spawn_if_not_busy<Fut>(&self, chat_id: ChatId, future: Fut) -> bool
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.busy.spawn_if_not_busy(chat_id, future)
    }
}

impl BotBusyStore {
    fn try_enter(self: &Arc<Self>, chat_id: ChatId) -> Option<BotBusyGuard> {
        let mut busy_chats = self.busy_chats.try_lock().ok()?;
        if !busy_chats.insert(chat_id) {
            return None;
        }
        Some(BotBusyGuard {
            chat_id,
            busy_chats: Arc::clone(self),
        })
    }

    fn spawn_if_not_busy<Fut>(self: &Arc<Self>, chat_id: ChatId, future: Fut) -> bool
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Some(guard) = self.try_enter(chat_id) else {
            return false;
        };
        tokio::spawn(async move {
            let _guard = guard;
            future.await;
        });
        true
    }
}

impl Drop for BotBusyGuard {
    fn drop(&mut self) {
        if let Ok(mut busy_chats) = self.busy_chats.busy_chats.lock() {
            busy_chats.remove(&self.chat_id);
        }
    }
}

impl TimerInputStore {
    pub(super) fn set(&mut self, chat_id: ChatId, mode: TimerInputMode, now: Instant) {
        self.prune(now);
        self.pending.insert(
            chat_id,
            PendingTimerInput {
                mode,
                expires_at: now + TIMER_INPUT_TTL,
            },
        );
    }

    pub(super) fn take(&mut self, chat_id: ChatId, now: Instant) -> Option<TimerInputMode> {
        self.prune(now);
        let pending = self.pending.remove(&chat_id)?;
        (pending.expires_at > now).then_some(pending.mode)
    }

    pub(super) fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| pending.expires_at > now);
    }
}

#[cfg(test)]
mod busy_tests {
    use super::*;

    #[test]
    fn busy_store_drops_second_request_without_queueing() {
        let busy = Arc::new(BotBusyStore::default());
        let chat = ChatId(10);

        let first = busy.try_enter(chat);
        assert!(first.is_some());
        assert!(busy.try_enter(chat).is_none());

        drop(first);
        assert!(busy.try_enter(chat).is_some());
    }

    #[test]
    fn busy_store_is_scoped_per_chat() {
        let busy = Arc::new(BotBusyStore::default());

        let _first = busy.try_enter(ChatId(10)).unwrap();
        assert!(busy.try_enter(ChatId(11)).is_some());
    }

    #[tokio::test]
    async fn spawned_busy_operation_drops_second_request_without_delayed_execution() {
        let busy = Arc::new(BotBusyStore::default());
        let chat = ChatId(10);
        let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel::<()>();
        let first_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_executed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        assert!(busy.spawn_if_not_busy(chat, {
            let first_started = Arc::clone(&first_started);
            async move {
                first_started.store(true, Ordering::SeqCst);
                let _ = release_first_rx.await;
            }
        }));

        while !first_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        assert!(!busy.spawn_if_not_busy(chat, {
            let second_executed = Arc::clone(&second_executed);
            async move {
                second_executed.store(true, Ordering::SeqCst);
            }
        }));

        let _ = release_first_tx.send(());
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(!second_executed.load(Ordering::SeqCst));
        assert!(busy.spawn_if_not_busy(chat, async {}));
    }
}

impl TimerMessageStore {
    pub(super) fn record(&mut self, chat_id: ChatId, message_id: MessageId) {
        let messages = self.messages.entry(chat_id).or_default();
        if !messages.contains(&message_id) {
            messages.push(message_id);
        }
    }

    pub(super) fn take_all(&mut self, chat_id: ChatId) -> Vec<MessageId> {
        self.messages.remove(&chat_id).unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn count(&self, chat_id: ChatId) -> usize {
        self.messages
            .get(&chat_id)
            .map(|messages| messages.len())
            .unwrap_or_default()
    }
}

impl ServerWizardStore {
    pub(super) fn start(&mut self, chat_id: ChatId, now: Instant) {
        self.prune(now);
        self.pending.remove(&chat_id);
        self.pending.insert(
            chat_id,
            PendingServerWizard {
                step: ServerWizardStep::Token,
                expires_at: now + SERVER_WIZARD_TTL,
            },
        );
    }

    pub(super) fn set_step(&mut self, chat_id: ChatId, step: ServerWizardStep, now: Instant) {
        self.prune(now);
        self.pending.insert(
            chat_id,
            PendingServerWizard {
                step,
                expires_at: now + SERVER_WIZARD_TTL,
            },
        );
    }

    pub(super) fn take_step(&mut self, chat_id: ChatId, now: Instant) -> Option<ServerWizardStep> {
        self.prune(now);
        let pending = self.pending.remove(&chat_id)?;
        (pending.expires_at > now).then_some(pending.step)
    }

    pub(super) fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| pending.expires_at > now);
    }
}

impl ServerEditStore {
    pub(super) fn set(&mut self, chat_id: ChatId, mode: ServerEditMode, now: Instant) {
        self.prune(now);
        self.pending.insert(
            chat_id,
            PendingServerEdit {
                mode,
                expires_at: now + SERVER_WIZARD_TTL,
            },
        );
    }

    pub(super) fn take(&mut self, chat_id: ChatId, now: Instant) -> Option<ServerEditMode> {
        self.prune(now);
        let pending = self.pending.remove(&chat_id)?;
        (pending.expires_at > now).then_some(pending.mode)
    }

    pub(super) fn cancel(&mut self, chat_id: ChatId) {
        self.pending.remove(&chat_id);
    }

    pub(super) fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| pending.expires_at > now);
    }

    #[cfg(test)]
    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl ConfirmationStore {
    pub(super) fn insert(&mut self, server_id: &str, now: Instant) -> String {
        self.prune(now);
        let nonce = next_nonce();
        self.pending.insert(
            nonce.clone(),
            PendingConfirmation {
                server_id: server_id.to_string(),
                expires_at: now + CONFIRM_TTL,
                used: false,
            },
        );
        nonce
    }

    pub(super) fn consume(&mut self, server_id: &str, nonce: &str, now: Instant) -> ConfirmConsume {
        let Some(pending) = self.pending.get_mut(nonce) else {
            self.prune(now);
            return ConfirmConsume::Missing;
        };
        if pending.server_id != server_id {
            self.prune(now);
            return ConfirmConsume::Mismatch;
        }
        if pending.expires_at <= now {
            self.pending.remove(nonce);
            self.prune(now);
            return ConfirmConsume::Expired;
        }
        if pending.used {
            self.prune(now);
            return ConfirmConsume::AlreadyUsed;
        }
        pending.used = true;
        self.prune(now);
        ConfirmConsume::Accepted
    }

    pub(super) fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| pending.expires_at > now);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ConfirmConsume {
    Accepted,
    Missing,
    Mismatch,
    Expired,
    AlreadyUsed,
}

pub(super) fn next_nonce() -> String {
    let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("{millis:x}{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_message_store_records_deduplicates_and_takes_messages() {
        let chat_id = ChatId(12345);
        let mut store = TimerMessageStore::default();
        store.record(chat_id, MessageId(10));
        store.record(chat_id, MessageId(10));
        store.record(chat_id, MessageId(11));

        assert_eq!(store.count(chat_id), 2);
        assert_eq!(store.take_all(chat_id), vec![MessageId(10), MessageId(11)]);
        assert_eq!(store.count(chat_id), 0);
    }

    #[test]
    fn server_edit_state_expires_and_can_be_cancelled() {
        let mut store = ServerEditStore::default();
        let chat_id = ChatId(12345);
        let now = Instant::now();
        store.set(
            chat_id,
            ServerEditMode::Name {
                server_id: "hk-01".to_string(),
            },
            now,
        );
        assert_eq!(store.pending_count(), 1);
        assert!(store
            .take(chat_id, now + SERVER_WIZARD_TTL + Duration::from_secs(1))
            .is_none());
        assert_eq!(store.pending_count(), 0);

        store.set(
            chat_id,
            ServerEditMode::Token {
                server_id: "hk-01".to_string(),
            },
            now,
        );
        store.cancel(chat_id);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn successful_token_edit_consumes_pending_state() {
        let mut store = ServerEditStore::default();
        let chat_id = ChatId(12345);
        let now = Instant::now();
        store.set(
            chat_id,
            ServerEditMode::Token {
                server_id: "hk-01".to_string(),
            },
            now,
        );

        assert!(matches!(
            store.take(chat_id, now + Duration::from_secs(1)),
            Some(ServerEditMode::Token { server_id }) if server_id == "hk-01"
        ));
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn timer_input_store_expires_and_consumes_once() {
        let mut store = TimerInputStore::default();
        let chat_id = ChatId(12345);
        let now = Instant::now();

        store.set(chat_id, TimerInputMode::New, now);
        assert!(matches!(
            store.take(chat_id, now + Duration::from_secs(1)),
            Some(TimerInputMode::New)
        ));
        assert!(store.take(chat_id, now + Duration::from_secs(2)).is_none());

        store.set(chat_id, TimerInputMode::New, now);
        assert!(store
            .take(chat_id, now + TIMER_INPUT_TTL + Duration::from_secs(1))
            .is_none());
    }

    #[test]
    fn nonce_expires_and_cannot_execute() {
        let mut store = ConfirmationStore::default();
        let now = Instant::now();
        let nonce = store.insert("hk-01", now);
        let result = store.consume("hk-01", &nonce, now + CONFIRM_TTL + Duration::from_secs(1));
        assert_eq!(result, ConfirmConsume::Expired);
    }

    #[test]
    fn nonce_is_single_use() {
        let mut store = ConfirmationStore::default();
        let now = Instant::now();
        let nonce = store.insert("hk-01", now);
        assert_eq!(
            store.consume("hk-01", &nonce, now + Duration::from_secs(1)),
            ConfirmConsume::Accepted
        );
        assert_eq!(
            store.consume("hk-01", &nonce, now + Duration::from_secs(2)),
            ConfirmConsume::AlreadyUsed
        );
    }
}

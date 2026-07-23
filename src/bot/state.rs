use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use teloxide::types::ChatId;
use tokio::sync::Mutex;

use crate::{
    config::{AppConfig, SecretToken},
    timer::TimerManager,
};

use super::formatting::GeoLabel;

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

#[derive(Clone, Debug)]
pub(super) enum ServerWizardStep {
    Name,
    Address {
        name: String,
    },
    Token {
        name: String,
        address: String,
        geo: GeoLabel,
        resolved_ip: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub(super) struct PendingServerWizard {
    step: ServerWizardStep,
    expires_at: Instant,
}

#[derive(Clone, Debug)]
pub(super) struct PendingServerDraft {
    pub(super) chat_id: ChatId,
    pub(super) name: String,
    pub(super) address: String,
    pub(super) token: SecretToken,
    pub(super) geo: GeoLabel,
    pub(super) resolved_ip: Option<String>,
    pub(super) expires_at: Instant,
}

#[derive(Default)]
pub(super) struct ServerWizardStore {
    pending: HashMap<ChatId, PendingServerWizard>,
    drafts: HashMap<String, PendingServerDraft>,
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

#[derive(Clone)]
pub(super) struct BotShared {
    pub(super) config: Arc<Mutex<AppConfig>>,
    pub(super) timer: Arc<Mutex<TimerManager>>,
    pub(super) confirmations: Arc<Mutex<ConfirmationStore>>,
    pub(super) timer_inputs: Arc<Mutex<TimerInputStore>>,
    pub(super) server_wizards: Arc<Mutex<ServerWizardStore>>,
    pub(super) server_edits: Arc<Mutex<ServerEditStore>>,
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

impl ServerWizardStore {
    pub(super) fn start(&mut self, chat_id: ChatId, now: Instant) {
        self.prune(now);
        self.pending.remove(&chat_id);
        self.drafts.retain(|_, draft| draft.chat_id != chat_id);
        self.pending.insert(
            chat_id,
            PendingServerWizard {
                step: ServerWizardStep::Name,
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

    pub(super) fn insert_draft(&mut self, draft: PendingServerDraft, now: Instant) -> String {
        self.prune(now);
        let nonce = next_nonce();
        self.drafts.insert(nonce.clone(), draft);
        nonce
    }

    pub(super) fn take_draft(&mut self, nonce: &str, now: Instant) -> Option<PendingServerDraft> {
        self.prune(now);
        let draft = self.drafts.remove(nonce)?;
        (draft.expires_at > now).then_some(draft)
    }

    pub(super) fn cancel_draft(&mut self, nonce: &str) {
        self.drafts.remove(nonce);
    }

    pub(super) fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| pending.expires_at > now);
        self.drafts.retain(|_, draft| draft.expires_at > now);
    }

    #[cfg(test)]
    pub(super) fn draft_count_for_chat(&self, chat_id: ChatId) -> usize {
        self.drafts
            .values()
            .filter(|draft| draft.chat_id == chat_id)
            .count()
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
    use crate::config::SecretToken;

    #[test]
    fn starting_server_wizard_clears_old_chat_token_drafts() {
        let chat_id = ChatId(12345);
        let mut store = ServerWizardStore::default();
        let now = Instant::now();
        store.insert_draft(
            PendingServerDraft {
                chat_id,
                name: "Server".to_string(),
                address: "example.com".to_string(),
                token: SecretToken::from_test_value("temporary-token"),
                geo: GeoLabel::unknown(),
                resolved_ip: None,
                expires_at: now + SERVER_WIZARD_TTL,
            },
            now,
        );

        store.start(chat_id, now + Duration::from_secs(1));

        assert_eq!(store.draft_count_for_chat(chat_id), 0);
    }

    #[test]
    fn expired_server_wizard_draft_is_pruned_with_token() {
        let chat_id = ChatId(12345);
        let mut store = ServerWizardStore::default();
        let now = Instant::now();
        store.insert_draft(
            PendingServerDraft {
                chat_id,
                name: "Server".to_string(),
                address: "example.com".to_string(),
                token: SecretToken::from_test_value("temporary-token"),
                geo: GeoLabel::unknown(),
                resolved_ip: None,
                expires_at: now + Duration::from_secs(1),
            },
            now,
        );

        store.prune(now + Duration::from_secs(2));

        assert_eq!(store.draft_count_for_chat(chat_id), 0);
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

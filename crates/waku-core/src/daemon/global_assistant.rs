//! Atomic creation of the daemon's one global conversation, without a provider call.
use super::*;

impl WakuBackend {
    pub(super) fn ensure_global_assistant(
        &self,
        provider: ProviderKind,
        model: Option<String>,
    ) -> anyhow::Result<ResponsePayload> {
        // A second client cannot allocate another workspace or replace the
        // assistant between the existence check and durable registration.
        let mut state = self.task_state.lock();
        let (project, session) =
            ensure_in_state(&self.task_store, &mut state, provider, model, || {
                Ok(crate::projectless::create_workspace(Some("Doki global assistant"))?.cwd)
            })?;
        // Explicit recreation clears the old deletion tombstone only after
        // the new singleton has been durably registered.
        self.removed_session_ids.lock().remove(&session.id);
        Ok(ResponsePayload::GlobalAssistant { project, session })
    }
}

fn ensure_in_state(
    store: &StateStore,
    state: &mut PersistedState,
    provider: ProviderKind,
    model: Option<String>,
    create_workspace: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<(Project, AgentSession)> {
    if let Some(index) = state
        .sessions
        .iter()
        .position(AgentSession::is_global_assistant)
    {
        store.hydrate(&mut state.sessions[index])?;
        let session = state.sessions[index].clone();
        let project = state
            .projects
            .iter()
            .find(|project| project.id == session.project_id)
            .cloned()
            .context("The assistant's private workspace is missing")?;
        return Ok((project, session));
    }
    let mut project = Project::from_path(create_workspace()?);
    project.name = Project::PROJECTLESS_NAME.to_owned();
    let mut session = AgentSession::global_assistant(project.id, provider);
    session.model = model.filter(|model| !model.trim().is_empty());
    state.projects.push(project.clone());
    state.push_session(session.clone());
    if let Err(error) = store.save(state) {
        state
            .sessions
            .retain(|session| !session.is_global_assistant());
        state.projects.retain(|entry| entry.id != project.id);
        // Leave the new private directory in place on a persistence error:
        // never remove a path that another process could have begun using.
        return Err(error.into());
    }
    Ok((project, session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_reuses_identity_and_persists_before_first_prompt() {
        let root = std::env::temp_dir().join(format!("doki-assistant-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new(root.join("app.db"));
        let mut state = PersistedState::fresh(root.clone());
        let (project, session) = ensure_in_state(
            &store,
            &mut state,
            ProviderKind::Codex,
            Some("model-a".into()),
            || Ok(root.join("private")),
        )
        .unwrap();
        assert_eq!(session.id, AgentSession::GLOBAL_ASSISTANT_ID);
        assert!(!session.has_started());
        let (_, again) = ensure_in_state(&store, &mut state, ProviderKind::Claude, None, || {
            panic!("must not allocate a second workspace")
        })
        .unwrap();
        assert_eq!(again.project_id, project.id);
        assert_eq!(again.provider, ProviderKind::Codex);
        assert_eq!(again.model.as_deref(), Some("model-a"));
        let mut restored = store.load().unwrap();
        let (_, reloaded) = ensure_in_state(&store, &mut restored, ProviderKind::Pi, None, || {
            panic!("restart must hydrate existing assistant")
        })
        .unwrap();
        assert_eq!(reloaded.id, session.id);
        assert!(reloaded.detail_loaded);
        assert_eq!(
            restored
                .sessions
                .iter()
                .filter(|session| session.is_global_assistant())
                .count(),
            1
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ensure_preserves_existing_conversation_and_permissions() {
        let root = std::env::temp_dir().join(format!("doki-assistant-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new(root.join("app.db"));
        let mut state = PersistedState::fresh(root.clone());
        ensure_in_state(&store, &mut state, ProviderKind::Codex, None, || {
            Ok(root.join("private"))
        })
        .unwrap();
        state
            .session_mut(AgentSession::GLOBAL_ASSISTANT_ID)
            .unwrap()
            .push_message(crate::model::MessageRole::User, "Keep this conversation");
        store.save(&mut state).unwrap();
        let (_, session) = ensure_in_state(
            &store,
            &mut state,
            ProviderKind::Claude,
            None,
            || unreachable!(),
        )
        .unwrap();
        assert_eq!(session.messages[0].content, "Keep this conversation");
        assert_eq!(session.runtime_mode, crate::model::RuntimeMode::Ask);
        assert!(session.task_id.is_none());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

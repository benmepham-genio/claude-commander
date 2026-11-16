use super::*;

#[tokio::test]
async fn deferred_program_survives_wake_and_automatic_recreation() {
    if !tmux_available().await {
        return;
    }
    for hibernate in [false, true] {
        let (_repo, path) = create_test_repo().await;
        let state_dir = TempDir::new().unwrap();
        let worktrees = TempDir::new().unwrap();
        let config = Config {
            resume_session: true,
            worktrees_dir: Some(worktrees.path().into()),
            ..Config::default()
        };
        let config = create_isolated_config_store(&state_dir, config);
        let store = create_isolated_store(&state_dir);
        let manager = SessionManager::new(config, store.clone(), "");
        let project = manager.add_project(path, None).await.unwrap();
        let id = manager
            .prepare_session(&project, "wake".into(), Some("sleep 60".into()), None)
            .await
            .unwrap();
        manager.finalize_session(&id, None, None).await.unwrap();
        let name = store
            .read()
            .await
            .get_session(&id)
            .unwrap()
            .tmux_session_name
            .clone();
        store
            .mutate(move |state| {
                state.get_session_mut(&id).unwrap().pending_program = Some("cat".into())
            })
            .await
            .unwrap();
        if hibernate {
            manager.hibernate_session(&id).await.unwrap();
        } else {
            manager.tmux.kill_session(&name).await.unwrap();
        }
        manager.ensure_attachable(&id).await.unwrap();
        {
            let state = store.read().await;
            let session = state.get_session(&id).unwrap();
            assert_eq!(
                session.program, "sleep 60",
                "wake must preserve the live program"
            );
            assert_eq!(session.pending_program.as_deref(), Some("cat"));
        }
        manager.restart_session(&id).await.unwrap();
        assert_eq!(store.read().await.get_session(&id).unwrap().program, "cat");
        manager.kill_session(&id, false).await.unwrap();
    }
}

/// Saving a different harness keeps the old pane alive; the next restart must
/// consume the pending command and launch fresh even when resume is enabled.
#[tokio::test]
async fn test_edit_session_program_restart_choices() {
    use claude_commander_core::api::CommanderService;
    use claude_commander_core::telemetry::FrontendInfo;
    use claude_commander_protocol::api::EditSession;
    if !tmux_available().await {
        return;
    }
    for restart in [false, true] {
        let (_repo_dir, repo_path) = create_test_repo().await;
        let state_dir = TempDir::new().unwrap();
        let worktrees = TempDir::new().unwrap();
        let fake_codex = state_dir.path().join("codex");
        std::fs::write(&fake_codex, "#!/bin/sh\nexec cat\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let new_program = fake_codex.to_string_lossy().into_owned();
        let config = Config {
            resume_session: true,
            worktrees_dir: Some(worktrees.path().to_path_buf()),
            ..Config::default()
        };
        let config_store = create_isolated_config_store(&state_dir, config);
        let store = create_isolated_store(&state_dir);
        let manager = SessionManager::new(config_store.clone(), store.clone(), "");
        let service = CommanderService::new(
            config_store,
            store.clone(),
            FrontendInfo::new("integration-test", "0.0.0"),
        );
        let project = manager.add_project(repo_path, None).await.unwrap();
        let sid = manager
            .prepare_session(&project, "edit-test".into(), Some("sleep 60".into()), None)
            .await
            .unwrap();
        manager.finalize_session(&sid, None, None).await.unwrap();
        let tmux_name = store
            .read()
            .await
            .get_session(&sid)
            .unwrap()
            .tmux_session_name
            .clone();
        let pid_before = manager
            .tmux
            .execute(&["display-message", "-p", "-t", &tmux_name, "#{pane_pid}"])
            .await
            .unwrap();
        service
            .edit_session(
                &sid,
                EditSession {
                    title: Some("edited".into()),
                    program: Some(new_program.clone()),
                    section: None,
                    keep_alive: Some(true),
                    base: None,
                    restart,
                },
            )
            .await
            .unwrap();
        {
            let state = store.read().await;
            let session = state.get_session(&sid).unwrap();
            assert_eq!(session.title, "edited");
            assert!(session.keep_alive);
            assert_eq!(session.status, SessionStatus::Running);
            if restart {
                assert_eq!(session.program, new_program);
                assert!(session.pending_program.is_none());
            } else {
                assert_eq!(session.program, "sleep 60");
                assert_eq!(
                    session.pending_program.as_deref(),
                    Some(new_program.as_str())
                );
            }
        }
        let pid_after = manager
            .tmux
            .execute(&["display-message", "-p", "-t", &tmux_name, "#{pane_pid}"])
            .await
            .unwrap();
        if restart {
            assert_ne!(pid_before, pid_after);
        } else {
            assert_eq!(pid_before, pid_after);
        }
        if !restart {
            service.restart_session(&sid).await.unwrap();
        }
        let tmux = {
            let state = store.read().await;
            let session = state.get_session(&sid).unwrap();
            assert_eq!(session.program, new_program);
            assert!(session.pending_program.is_none());
            session.tmux_session_name.clone()
        };
        let command = manager
            .tmux
            .execute(&[
                "display-message",
                "-p",
                "-t",
                &tmux,
                "#{pane_start_command}",
            ])
            .await
            .unwrap();
        assert!(
            !command.contains("resume --last"),
            "new harness must launch fresh: {command}"
        );
        assert!(manager.tmux.session_exists(&tmux).await.unwrap());
        assert!(!manager.tmux.is_pane_dead(&tmux).await.unwrap());
        manager.kill_session(&sid, false).await.unwrap();
    }
}

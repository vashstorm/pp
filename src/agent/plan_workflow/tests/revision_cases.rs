use super::*;

#[test]
fn plan_external_import_registers_revisions_and_respects_readonly_statuses() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);

        // Unchanged content keeps the revision and stays Draft.
        let review = agent.read_plan().unwrap();
        assert_eq!(review.document.revision, 1);
        assert_eq!(review.document.status, PlanStatus::Draft);
        assert_eq!(review.token.content, PLAN_CONTENT);
        assert_eq!(review.token.revision, 1);
        assert_eq!(review.token.session_id, agent.session_id().unwrap());

        // An external edit becomes the next revision and reaches Ready
        // only with a genuine task list.
        let edited = format!("{PLAN_CONTENT}\nextra context\n");
        write_plan_file(&agent, &edited);
        let review = agent.read_plan().unwrap();
        assert_eq!(review.document.revision, 2);
        assert_eq!(review.document.status, PlanStatus::Ready);
        assert_eq!(review.token.content, edited);

        // Missing task lists fall back to Draft.
        write_plan_file(&agent, "# Create hello\n\nno tasks here\n");
        let review = agent.read_plan().unwrap();
        assert_eq!(review.document.revision, 3);
        assert_eq!(review.document.status, PlanStatus::Draft);

        // A missing file is an error, never a journal fallback.
        let bound_path = agent
            .workflow
            .plan
            .as_ref()
            .unwrap()
            .path()
            .expect("registered plan has a path");
        std::fs::remove_file(dir.path().join(bound_path)).unwrap();
        assert!(agent.read_plan().is_err());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().revision,
            3,
            "failed import never mutates the snapshot"
        );

        // Executed plans refresh their displayed snapshot (execution
        // progress) without regaining approval eligibility.
        let document = agent.workflow.plan.as_mut().unwrap();
        document.status = PlanStatus::Executed;
        write_plan_file(&agent, "- [ ] changed externally\n");
        let review = agent.read_plan().unwrap();
        assert_eq!(review.document.revision, 4);
        assert_eq!(review.document.status, PlanStatus::Executed);
        assert_eq!(review.document.content, "- [ ] changed externally\n");

        // A revision-0 binding has no saved path: neither an origin
        // requirement nor a hand-written import applies.
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, ORIGIN);
        agent.workflow = plan_workflow("", ORIGIN, PlanStatus::Draft);
        let error = err_message(agent.read_plan());
        assert_eq!(error, "No saved plan.");

        // Without origin the review is refused before any import.
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow("", "", PlanStatus::Draft);
        let error = err_message(agent.read_plan());
        assert_eq!(error, "No saved plan.");
    }
}

#[test]
fn plan_token_validation_denies_stale_missing_and_pending() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let token = agent.read_plan().unwrap().token;
        let queue = crate::agent::input::InputQueue::default();
        agent.validate_plan_token(&token, &queue).unwrap();

        // Pending input denies approval without touching any state.
        queue.begin_run().unwrap();
        queue
            .enqueue(
                crate::agent::input::PendingKind::Steering,
                "late question".into(),
                None,
                false,
            )
            .unwrap();
        let error = agent
            .validate_plan_token(&token, &queue)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Apply pending input before approving the plan.");
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );
        assert!(queue.claim_steering(false).unwrap().len() == 1);

        // Changed bytes report the exact changed message without a model
        // request; approval itself imports the new revision so the next
        // ReadPlan merely refreshes the token.
        let edited = format!("{PLAN_CONTENT}\nexternal\n");
        write_plan_file(&agent, &edited);
        let error = agent
            .validate_plan_token(&token, &queue)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Plan changed; review the new revision before executing."
        );
        assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 1);
        let error = err_message(agent.approve_plan(
            token.clone(),
            ExecutionContext::Keep,
            &queue,
            &CancellationToken::new(),
            &mut |_| {},
        ));
        assert_eq!(
            error,
            "Plan changed; review the new revision before executing."
        );
        assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);
        let review = agent.read_plan().unwrap();
        assert_eq!(review.document.revision, 2);
        agent.validate_plan_token(&review.token, &queue).unwrap();

        // Stale session and stale revision tokens are no longer current.
        let mut stale = review.token.clone();
        stale.session_id = "other-session".into();
        assert_eq!(
            agent
                .validate_plan_token(&stale, &queue)
                .unwrap_err()
                .to_string(),
            NOT_CURRENT
        );
        let mut stale = review.token.clone();
        stale.revision = 1;
        assert_eq!(
            agent
                .validate_plan_token(&stale, &queue)
                .unwrap_err()
                .to_string(),
            NOT_CURRENT
        );

        // A token bound to another path (rename/move) is no longer current.
        let mut stale = review.token.clone();
        stale.path = ".pp/plans/moved-away.md".into();
        assert_eq!(
            agent
                .validate_plan_token(&stale, &queue)
                .unwrap_err()
                .to_string(),
            NOT_CURRENT
        );

        // A deleted file blocks approval instead of using the snapshot.
        let bound_path = agent
            .workflow
            .plan
            .as_ref()
            .unwrap()
            .path()
            .expect("registered plan has a path");
        std::fs::remove_file(dir.path().join(bound_path)).unwrap();
        assert!(agent.validate_plan_token(&review.token, &queue).is_err());
        assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);

        // The cleanup gate blocks validation and ordinary tasks alike.
        agent.plan_cleanup_error = Some(
            "MCP servers could not be stopped for Plan mode: boom; run /plan again to retry."
                .into(),
        );
        let error = agent
            .validate_plan_token(&review.token, &queue)
            .unwrap_err()
            .to_string();
        assert!(error.contains("run /plan again to retry"));
        let outcome = agent.run(
            "research task",
            &CancellationToken::new(),
            None,
            &mut |_| {},
        );
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "MCP servers could not be stopped for Plan mode: boom; run /plan again to retry."
        );

        // A successful retry through /plan clears the gate.
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert!(agent.plan_cleanup_error.is_none());
    }
}

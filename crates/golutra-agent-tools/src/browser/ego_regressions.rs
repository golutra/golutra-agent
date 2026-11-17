//! 针对官方 SDK 生命周期与错误契约的回归；协议替身不能替代实机验收。
use super::*;

#[tokio::test]
async fn retained_goal_reclaims_same_space_with_new_labels_and_preserves_user_tabs() {
    let fixture = Fixture::new();
    let first = fixture.call("browser_run", json!({"code":"await page.goto('https://example.test/first'); await (await task.newPage()).goto('https://example.test/result');"})).await;
    assert!(!first.is_error);
    assert!(
        !fixture
            .call("browser_finish", json!({"keep":["p2"]}))
            .await
            .is_error
    );
    let db_path = fixture.root.path().join("spaces.json");
    let mut db: Value = read_json(&db_path).unwrap();
    db["spaces"][0]["unmanaged"]
        .as_array_mut()
        .unwrap()
        .push(json!({"url":"https://user.test/private","openedBy":"unknown"}));
    std::fs::write(&db_path, serde_json::to_vec(&db).unwrap()).unwrap();
    let stale = fixture
        .call(
            "browser_run",
            json!({"page":"p2","code":"throw Error('stale')"}),
        )
        .await;
    assert_eq!(stale.structured_facts["code"], "browser_labels_expired");
    let next = fixture
        .call(
            "browser_run",
            json!({"code":"console.log(await page.url());"}),
        )
        .await;
    assert!(!next.is_error, "{next:?}");
    assert_eq!(
        next.structured_facts["space_id"],
        first.structured_facts["space_id"]
    );
    assert_eq!(next.structured_facts["browser"]["resumed_finished"], true);
    assert_eq!(
        next.structured_facts["browser"]["pages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_ne!(next.structured_facts["default_page"], "p2");
    assert!(next.content.contains("https://example.test/result"));
    let again = fixture
        .call(
            "browser_run",
            json!({"code":"console.log(await page.url());"}),
        )
        .await;
    assert!(!again.is_error);
    assert!(again.content.contains("https://example.test/result"));
    let db: Value = read_json(&db_path).unwrap();
    assert_eq!(db["claims"], 1);
    assert_eq!(db["spaces"][0]["unmanaged"][0]["openedBy"], "unknown");
    let finished = fixture
        .call("browser_finish", json!({"release":true}))
        .await;
    assert!(!finished.is_error);
    assert_eq!(
        finished.structured_facts["browser"]["completion"]["preservedUnmanagedCount"],
        1
    );
    assert_eq!(finished.structured_facts["reclaimable"], false);
    assert_eq!(
        finished.structured_facts["browser"]["completion"]["closedSpace"],
        false
    );
}

#[tokio::test]
async fn retained_goal_missing_space_or_user_control_never_creates_replacement() {
    for missing in [true, false] {
        let fixture = Fixture::new();
        assert!(
            !fixture
                .call("browser_run", json!({"code":"return 1"}))
                .await
                .is_error
        );
        assert!(!fixture.call("browser_finish", json!({})).await.is_error);
        let db_path = fixture.root.path().join("spaces.json");
        let mut db: Value = read_json(&db_path).unwrap();
        if missing {
            db["spaces"] = json!([]);
        } else {
            db["spaces"][0]["ownership"] = json!("agentDelegatedToUser");
            db["spaces"][0]["control"] = json!("user");
        }
        std::fs::write(&db_path, serde_json::to_vec(&db).unwrap()).unwrap();
        let result = fixture
            .call(
                "browser_run",
                json!({"code":"throw Error('must not execute')"}),
            )
            .await;
        assert!(result.is_error);
        assert_eq!(
            result.structured_facts["browser"]["code"],
            if missing {
                "EGO_SPACE_MISSING"
            } else {
                "EGO_TASK_SPACE_USER_IN_CONTROL"
            }
        );
        let db: Value = read_json(&db_path).unwrap();
        assert_eq!(db["next"], 2);
        assert!(db.get("claims").is_none());
    }
}

#[tokio::test]
async fn late_effects_survive_observation_until_explicit_reload() {
    for fields in [
        "executionStopped:false",
        "mayHaveLateEffects:true",
        "code:'EGO_PAGE_EVALUATION_TIMED_OUT'",
    ] {
        let fixture = Fixture::new();
        let code = format!(
            "await page.goto('https://example.test/changed'); await task.newPage(); const e=Object.assign(new Error('evaluation timeout'),{{{fields}}}); throw e;"
        );
        let failed = fixture.call("browser_run", json!({"code":code})).await;
        assert!(failed.is_error);
        assert_eq!(failed.structured_facts["requires_reload"], true);
        let observed = fixture
            .call(
                "browser_run",
                json!({"observation":"snapshot","scope":"full_page"}),
            )
            .await;
        assert!(!observed.is_error);
        assert_eq!(observed.structured_facts["phase"], "outcome_unknown");
        assert!(observed.content.contains("full_page"));
        let blocked = fixture
            .call(
                "browser_run",
                json!({"code":"return 'must not run'","new_goal":true}),
            )
            .await;
        assert_eq!(blocked.structured_facts["code"], "browser_outcome_unknown");
        let recovered = fixture
            .call(
                "browser_run",
                json!({"observation":"snapshot","recovery":"reload"}),
            )
            .await;
        assert!(!recovered.is_error, "{recovered:?}");
        assert_eq!(recovered.structured_facts["phase"], "ready");
        assert_eq!(recovered.structured_facts["requires_reload"], false);
        let db: Value = read_json(&fixture.root.path().join("spaces.json")).unwrap();
        assert_eq!(db["reloads"], 2);
        assert!(
            !fixture
                .call("browser_run", json!({"code":"return 'recovered'"}))
                .await
                .is_error
        );
    }
}

#[tokio::test]
async fn confirmed_stopped_evaluation_can_recover_by_observation() {
    let fixture = Fixture::new();
    let failed=fixture.call("browser_run",json!({"code":"throw Object.assign(new Error('stopped'), {code:'EGO_PAGE_EVALUATION_TIMED_OUT',executionStopped:true,mayHaveLateEffects:false});"})).await;
    assert!(failed.is_error);
    assert_eq!(failed.structured_facts["requires_reload"], false);
    let recovered = fixture
        .call(
            "browser_run",
            json!({"observation":"snapshot","scope":"subtree","root":"@12"}),
        )
        .await;
    assert!(!recovered.is_error);
    assert_eq!(recovered.structured_facts["phase"], "ready");
    assert!(recovered.content.contains("@12"));
}

#[tokio::test]
async fn control_return_and_failed_reload_cannot_erase_late_effects() {
    let fixture = Fixture::new();
    let failed = fixture.call("browser_run", json!({"code":"await task.handOff(); throw Object.assign(new Error('user took control'),{code:'EGO_TASK_SPACE_USER_IN_CONTROL',mayHaveLateEffects:true});"})).await;
    assert_eq!(failed.structured_facts["phase"], "waiting_for_user");
    let blocked = fixture
        .call(
            "browser_run",
            json!({"code":"throw Error('unsafe')","resume_after_user":true}),
        )
        .await;
    assert_eq!(blocked.structured_facts["code"], "browser_outcome_unknown");
    std::fs::write(fixture.root.path().join("reload-fails"), "").unwrap();
    let reload = fixture
        .call(
            "browser_run",
            json!({"observation":"snapshot","recovery":"reload","resume_after_user":true}),
        )
        .await;
    assert!(reload.is_error);
    assert_eq!(reload.structured_facts["requires_reload"], true);
    assert_eq!(reload.structured_facts["phase"], "outcome_unknown");
}

#[tokio::test]
async fn interrupted_claim_recovers_without_claiming_twice_or_losing_labels() {
    let fixture = Fixture::new();
    assert!(
        !fixture
            .call(
                "browser_run",
                json!({"code":"await page.goto('https://example.test/retained');"})
            )
            .await
            .is_error
    );
    assert!(!fixture.call("browser_finish", json!({})).await.is_error);
    // 模拟原生 claim 成功后、adopt/本地状态确认前的进程中断。
    let backend = fixture.backend();
    let call = request(
        fixture.session,
        "browser_run",
        json!({"observation":"snapshot"}),
    );
    let lease = StateLease::acquire(&backend.state_root, &call).unwrap();
    let mut state = lease
        .read(&call, &backend.workspace, &backend.config.executable)
        .unwrap();
    state.phase = "running".into();
    state.reclaim_pending = true;
    lease.write(&state).unwrap();
    drop(lease);
    let db_path = fixture.root.path().join("spaces.json");
    let mut db: Value = read_json(&db_path).unwrap();
    db["spaces"][0]["ownership"] = json!("agent");
    db["spaces"][0]["control"] = json!("agent");
    std::fs::write(&db_path, serde_json::to_vec(&db).unwrap()).unwrap();
    let observed = fixture
        .call("browser_run", json!({"observation":"snapshot"}))
        .await;
    assert!(!observed.is_error, "{observed:?}");
    assert_eq!(observed.structured_facts["phase"], "outcome_unknown");
    assert!(observed.content.contains("https://example.test/retained"));
    let db: Value = read_json(&db_path).unwrap();
    assert!(db.get("claims").is_none());
    assert_eq!(db["next"], 2);
    let recovered = fixture
        .call(
            "browser_run",
            json!({"observation":"snapshot","recovery":"reload"}),
        )
        .await;
    assert_eq!(recovered.structured_facts["phase"], "ready");
}

#[tokio::test]
async fn guide_rejects_truncation_and_version_changes_between_pages() {
    let fixture = Fixture::new();
    let guide = fixture.call("browser_help", json!({})).await;
    assert!(!guide.is_error);
    assert_eq!(guide.structured_facts["guide_complete"], true);
    let id = guide.structured_facts["guide_id"].clone();
    let next = fixture
        .call("browser_help", json!({"offset":6000,"guide_id":id}))
        .await;
    assert!(!next.is_error);
    let changed = fixture
        .call(
            "browser_help",
            json!({"offset":6000,"guide_id":"different-version"}),
        )
        .await;
    assert!(changed.is_error);
    assert_eq!(changed.structured_facts["guide_matches"], false);
    std::fs::write(fixture.root.path().join("incomplete-guide"), "").unwrap();
    let truncated = fixture.call("browser_help", json!({})).await;
    assert!(truncated.is_error);
    assert_eq!(truncated.structured_facts["guide_complete"], false);
    assert_eq!(truncated.structured_facts["next_offset"], Value::Null);
}

#[test]
fn fixed_observation_options_reject_code_injection_and_incompatible_combinations() {
    for args in [
        json!({"code":"return 1","scope":"full_page"}),
        json!({"observation":"screenshot","scope":"full_page"}),
        json!({"observation":"snapshot","scope":"subtree"}),
        json!({"observation":"snapshot","scope":"subtree","root":"@1);throw Error()"}),
        json!({"observation":"snapshot","scope":"full_page","root":"@1"}),
        json!({"observation":"snapshot","scope":"subtree","root":"@1","recovery":"reload"}),
        json!({"code":"return 1","recovery":"reload"}),
        json!({"observation":"snapshot","new_goal":true}),
    ] {
        assert!(ego_arguments::validate("run", &args).is_err(), "{args}");
    }
}

//! Child inspection keeps the parent session and controller attached.
use super::*;

async fn page(
    transport: &RuntimeTransport,
    session: SessionId,
    cursor: u64,
) -> miette::Result<Value> {
    transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id: session,
            task_id: None,
            kind: RuntimeQueryKind::Subagents,
            requester: ActorKind::User,
            cursor: Some(cursor),
            timestamp: chrono::Utc::now(),
        })
        .await
        .map_err(|error| miette::miette!("{error}"))
}

fn label(child: &Value) -> String {
    let status = if child["waiting_for_user"] == true {
        "等待补充信息"
    } else {
        match child["status"].as_str().unwrap_or("unknown") {
            "running" => "运行中",
            "completed" => "已完成",
            "partial" => "部分完成",
            "cancelled" => "已取消",
            "failed" => "失败",
            "blocked" => "受阻",
            "paused" => "已暂停",
            "uncertain" => "结果待核实",
            other => other,
        }
    };
    format!(
        "{} · {} · {}",
        child["name"].as_str().unwrap_or("agent"),
        status,
        child["task"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect::<String>()
    )
}

impl TuiApp {
    pub(super) async fn execute_agents_command(
        &mut self,
        transport: &RuntimeTransport,
        action: &str,
        target: Option<&str>,
        task: Option<&str>,
    ) -> miette::Result<()> {
        if matches!(action, "list" | "page") {
            let cursor = target
                .map(str::parse::<u64>)
                .transpose()
                .map_err(|_| miette::miette!("分页位置必须为 /agents 返回的数字"))?
                .unwrap_or(0);
            let result = page(transport, self.session_id, cursor).await?;
            let mut lines = result["children"]
                .as_array()
                .into_iter()
                .flatten()
                .map(label)
                .collect::<Vec<_>>();
            if lines.is_empty() {
                lines.push("当前会话没有子代理。".into());
            }
            if let Some(next) = result["next_cursor"].as_u64() {
                lines.push(format!("下一页：/agents page {next}"));
            }
            lines.push("查看：/agents show <名称>；取消：/agents cancel <名称>；继续：/agents continue <名称> <任务>；打开：/agents open <名称>".into());
            self.push_system_message("子代理", lines);
            return Ok(());
        }
        let target = target.unwrap_or_default();
        let mut cursor = 0;
        let child = loop {
            let result = page(transport, self.session_id, cursor).await?;
            if let Some(child) = result["children"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|child| child["name"] == target || child["child_session_id"] == target)
            {
                break child.clone();
            }
            match result["next_cursor"].as_u64() {
                Some(next) if next > cursor => cursor = next,
                _ => {
                    self.push_system_message(
                        "子代理",
                        vec!["未找到属于当前会话的子代理，请用 /agents 查看名称。".into()],
                    );
                    return Ok(());
                }
            }
        };
        let session = child["child_session_id"]
            .as_str()
            .and_then(|id| id.parse::<SessionId>().ok())
            .ok_or_else(|| miette::miette!("子代理会话记录无效"))?;
        match action {
            "show" => {
                let projection = transport
                    .query(RuntimeQuery {
                        query_id: QueryId::new(),
                        session_id: session,
                        task_id: child["task_id"].as_str().and_then(|id| id.parse().ok()),
                        kind: RuntimeQueryKind::UserProjection,
                        requester: ActorKind::User,
                        cursor: None,
                        timestamp: chrono::Utc::now(),
                    })
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                let mut lines = vec![label(&child)];
                for question in child["questions"].as_array().into_iter().flatten() {
                    if let Some(text) = question["question"].as_str() {
                        lines.push(text.into());
                    }
                    for option in question["options"].as_array().into_iter().flatten() {
                        if let Some(text) = option["label"].as_str() {
                            lines.push(format!("  · {text}"));
                        }
                    }
                }
                if let Some(message) = projection["final_message"].as_str() {
                    lines.extend(message.lines().map(str::to_owned));
                }
                for step in projection["visible_steps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .rev()
                    .take(8)
                {
                    if let Some(summary) =
                        step["summary"].as_str().filter(|value| !value.is_empty())
                    {
                        lines.push(summary.into());
                    }
                }
                if child["pending_messages"].as_u64().unwrap_or(0) > 0 {
                    lines.push(format!("待处理消息：{}", child["pending_messages"]));
                }
                self.push_system_message("子代理详情", lines);
            }
            "cancel" => {
                if self.controller_mode == RuntimeControllerMode::Observer {
                    self.push_system_message(
                        "子代理",
                        vec!["当前为只读观察，接管父会话后才能取消。".into()],
                    );
                    return Ok(());
                }
                let ack = transport
                    .send_command(session_command(
                        self.session_id,
                        SessionCommandKind::Abort,
                        json!({"child_session_id":session,"expected_task_id":child["task_id"]}),
                    ))
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                self.push_system_message(
                    "子代理",
                    vec![if ack.accepted {
                        "已请求取消所选子代理。".into()
                    } else {
                        compact_ack_reason(&ack.reason)
                    }],
                );
                self.last_control_ack = Some(ack);
            }
            "continue" => {
                let prompt = format!(
                    "请让子代理「{}」（已有会话 {}）继续下面的任务，沿用该子会话：\n{}",
                    child["name"].as_str().unwrap_or("agent"),
                    session,
                    task.unwrap_or_default()
                );
                // The parent admits the follow-up under its current budget and
                // permissions; the UI does not launch an ungoverned child loop.
                self.submit_runtime_prompt_with_mode(transport, prompt, has_active_task(self))
                    .await?;
            }
            "open" => {
                if has_active_task(self) {
                    self.push_system_message(
                        "子代理",
                        vec![
                            "主任务仍在运行，可用 /agents show 查看；主任务结束后可以打开子会话。"
                                .into(),
                        ],
                    );
                    return Ok(());
                }
                let thread = child["child_thread_id"]
                    .as_str()
                    .and_then(|id| id.parse::<ThreadId>().ok())
                    .ok_or_else(|| miette::miette!("子线程记录无效"))?;
                self.push_system_message(
                    "子代理",
                    vec![format!("返回主会话：/resume {}", self.thread_id)],
                );
                self.resume_thread_from_command(transport, thread).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subagent_commands_preserve_names_and_guard_task_bound_mutations() {
        for command in [
            "/agents",
            "/agents page 32",
            "/agents show reviewer",
            "/agents cancel reviewer",
            "/agents open reviewer",
            "/agents continue reviewer 后续任务",
        ] {
            assert!(
                matches!(
                    parse_slash_input(command),
                    SlashInput::Command(SlashCommand::Agents { .. })
                ),
                "{command}"
            );
        }
        assert!(matches!(
            parse_slash_input("/agents continue reviewer"),
            SlashInput::Error(_)
        ));
        assert!(matches!(
            parse_slash_input("/agents cancel"),
            SlashInput::Error(_)
        ));
    }
    #[test]
    fn subagent_names_can_be_quoted_without_rewriting_the_followup() {
        assert!(
            matches!(parse_slash_input("/agents continue \"代码 审查\" don't change  'this'"), SlashInput::Command(SlashCommand::Agents {target:Some(name),task:Some(task),..}) if name=="代码 审查" && task=="don't change  'this'")
        );
        assert!(
            matches!(parse_slash_input("/agents show \"代码 审查\""), SlashInput::Command(SlashCommand::Agents {target:Some(name),..}) if name=="代码 审查")
        );
        assert!(matches!(
            parse_slash_input("/agents show \"unfinished"),
            SlashInput::Error(_)
        ));
    }
    #[test]
    fn subagent_lines_show_task_and_state_without_internal_metadata() {
        let value = json!({"name":"审查","status":"running","task":"检查锁","child_session_id":"secret-id","background":"internal"});
        assert_eq!(label(&value), "审查 · 运行中 · 检查锁");
    }
}

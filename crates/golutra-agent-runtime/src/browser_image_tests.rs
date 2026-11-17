use super::*;
use base64::Engine;
use golutra_agent_tools::ExternalToolImage;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a8l8AAAAASUVORK5CYII=";

#[derive(Debug)]
struct ImageBackend(golutra_agent_core::ToolContract);

#[async_trait]
impl ExternalToolBackend for ImageBackend {
    fn contracts(&self) -> Vec<golutra_agent_core::ToolContract> {
        vec![self.0.clone()]
    }
    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        HashMap::from([(
            self.0.tool_name.clone(),
            ToolCapabilities {
                available_in_coding_profile: true,
                parallel_read_safe: false,
                coding_profile_hidden_arguments: Vec::new(),
            },
        )])
    }
    async fn call(
        &self,
        _: &ToolRequest,
        _: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        Ok(ExternalToolOutput {
            summary: "Screenshot captured".to_owned(),
            content: "Observed page".to_owned(),
            structured_facts: json!({}),
            is_error: false,
            images: vec![ExternalToolImage {
                mime_type: "image/png".to_owned(),
                bytes: base64::engine::general_purpose::STANDARD
                    .decode(PNG)
                    .unwrap(),
            }],
        })
    }
}

struct ImageProvider {
    inner: MockProvider,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait]
impl LlmProvider for ImageProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.complete(request).await
    }
    fn contract(&self) -> golutra_agent_core::ProviderContract {
        self.inner.contract()
    }
}

#[tokio::test]
async fn browser_image_reaches_next_model_round_after_paired_tool_result() {
    let workspace = tempdir().unwrap();
    let mut contract = golutra_agent_tools::ToolRegistry::p0_default()
        .contract("read_file")
        .unwrap()
        .clone();
    contract.tool_name = "browser_run".to_owned();
    contract.side_effect_type = golutra_agent_core::SideEffectType::ExternalSystem;
    contract.input_schema = json!({"type":"object","properties":{},"additionalProperties":false});
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap())
        .with_unrestricted_access(true)
        .with_external_backend(Arc::new(ImageBackend(contract)))
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ImageProvider {
        inner: MockProvider::tool_call("browser_run", json!({})),
        requests: requests.clone(),
    };
    AgentLoop::new(provider, ContextBuilder::default(), executor)
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "Observe the page screenshot and describe it".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["browser_run".to_owned()],
        })
        .await
        .unwrap();
    let requests = requests.lock().unwrap();
    assert!(requests.len() >= 2);
    let messages = &requests[1].messages;
    let image_index = messages
        .iter()
        .position(|message| !message.metadata.images.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "model must receive actual image; tool results: {:?}",
                messages
                    .iter()
                    .filter(|message| message.role == ProviderRole::Tool)
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(messages[image_index - 1].role, ProviderRole::Tool);
    assert_eq!(
        messages[image_index - 1].tool_name.as_deref(),
        Some("browser_run")
    );
    assert_eq!(messages[image_index].metadata.images[0].base64, PNG);
    let restored: Vec<ProviderMessage> =
        serde_json::from_slice(&serde_json::to_vec(messages).unwrap()).unwrap();
    assert_eq!(&restored, messages);
    let estimate = golutra_agent_context::estimate_message_token(&messages[image_index]);
    assert!(
        estimate >= 4096,
        "vision must have a nonzero context budget"
    );
}

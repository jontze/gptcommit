use anyhow::{anyhow, bail, Ok, Result};
use std::fmt;
use std::fmt::Debug;
use std::time::Duration;

use async_trait::async_trait;

use reqwest::{tls, Proxy};
use tiktoken_rs::{async_openai::get_chat_completion_max_tokens, get_completion_max_tokens};

use crate::{settings::OpenAISettings, util::HTTP_USER_AGENT};
use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessageArgs, CreateChatCompletionRequestArgs,
        CreateCompletionRequestArgs, Role,
    },
    Client,
};

use super::llm_client::LlmClient;
const COMPLETION_TOKEN_LIMIT: usize = 100;

pub(crate) struct OpenAIClient {
    model: String,
    client: Client<OpenAIConfig>,
}

impl Debug for OpenAIClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAIClient")
            .field("model", &self.model)
            .finish()
    }
}

impl OpenAIClient {
    pub(crate) fn new(settings: OpenAISettings) -> Result<Self, anyhow::Error> {
        let api_base = settings.api_base.unwrap_or_default();
        let api_key = settings.api_key.unwrap_or_default();
        if api_key.is_empty() {
            bail!("No OpenAI API key found. Please provide a valid API key.");
        }
        let openai_config = if api_base.is_empty() {
            OpenAIConfig::new().with_api_key(api_key)
        } else {
            OpenAIConfig::new()
                .with_api_base(&api_base)
                .with_api_key(api_key)
        };
        let mut openai_client = Client::<OpenAIConfig>::with_config(openai_config);
        // TODO make configurable
        let mut http_client = reqwest::Client::builder()
            .gzip(true)
            .brotli(true)
            .timeout(Duration::from_secs(60))
            .user_agent(HTTP_USER_AGENT);

        if api_base.is_empty() {
            // Optimized HTTP client
            http_client = http_client
                .http2_prior_knowledge()
                .https_only(true)
                .http2_adaptive_window(true)
                .tcp_keepalive(Duration::from_secs(60))
                .http2_keep_alive_interval(Duration::from_secs(60))
                .http2_keep_alive_while_idle(true)
                .min_tls_version(tls::Version::TLS_1_2);
        }
        let model = settings.model.unwrap_or_default();
        if api_base.is_empty() && model.is_empty() {
            bail!("No OpenAI model configured. Please choose a valid model to use.");
        }

        if let Some(proxy) = settings.proxy {
            if !proxy.is_empty() {
                http_client = http_client.proxy(Proxy::all(proxy)?);
            }
        }
        openai_client = openai_client.with_http_client(http_client.build()?);

        if settings.retries.unwrap_or_default() > 0 {
            let backoff = backoff::ExponentialBackoffBuilder::new()
                .with_max_elapsed_time(Some(std::time::Duration::from_secs(60)))
                .build();
            openai_client = openai_client.with_backoff(backoff);
        }
        Ok(Self {
            model,
            client: openai_client,
        })
    }

    pub(crate) fn should_use_chat_completion(model: &str) -> bool {
        model.to_lowercase().starts_with("gpt-4")
            || model.to_lowercase().starts_with("gpt-3.5-turbo")
    }

    pub(crate) async fn get_completions(&self, prompt: &str) -> Result<String> {
        let prompt_token_limit = get_completion_max_tokens(&self.model, prompt)?;

        if prompt_token_limit < COMPLETION_TOKEN_LIMIT {
            let error_msg =
"Skipping... The diff is too large for the current model. Consider using a model with a larger context window.".to_string();
            warn!("{}", error_msg);
            bail!(error_msg)
        }
        // Create request using builder pattern
        let request = CreateCompletionRequestArgs::default()
            .model(&self.model)
            .prompt(prompt)
            .max_tokens(prompt_token_limit as u16)
            .temperature(0.5)
            .top_p(1.)
            .frequency_penalty(0.)
            .presence_penalty(0.)
            .build()?;

        debug!("Sending request to OpenAI:\n{:?}", request);

        let response = self
            .client
            .completions() // Get the API "group" (completions, images, etc.) from the client
            .create(request) // Make the API call in that "group"
            .await?;

        let completion = response
            .choices
            .first()
            .ok_or(anyhow!("No completion results returned from OpenAI."))
            .map(|c| c.text.clone());

        completion
    }

    pub(crate) async fn get_chat_completions(&self, prompt: &str) -> Result<String> {
        let messages = [ChatCompletionRequestMessageArgs::default()
            .role(Role::User)
            .content(prompt)
            .build()?];
        let prompt_token_limit = get_chat_completion_max_tokens(&self.model, &messages)?;

        if prompt_token_limit < COMPLETION_TOKEN_LIMIT {
            let error_msg =
                "skipping... diff is too large for the model. Consider using a model with a larger context window.".to_string();
            warn!("{}", error_msg);
            bail!(error_msg)
        }

        let request = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages(messages)
            .build()?;

        let response = self.client.chat().create(request).await?;

        if let Some(choice) = response.choices.into_iter().next() {
            debug!(
                "{}: Role: {}  Content: {}",
                choice.index,
                choice.message.role,
                choice.message.content.clone().unwrap_or_default()
            );

            return choice
                .message
                .content
                .ok_or(anyhow!("No completion results returned from OpenAI."));
        }

        bail!("No completion results returned from OpenAI.")
    }
}

#[async_trait]
impl LlmClient for OpenAIClient {
    /// Sends a request to OpenAI's API to get a text completion.
    /// It takes a prompt as input, and returns the completion.
    async fn completions(&self, prompt: &str) -> Result<String> {
        let completion = if OpenAIClient::should_use_chat_completion(&self.model) {
            self.get_chat_completions(prompt).await?
        } else {
            self.get_completions(prompt).await?
        };
        Ok(completion.trim().to_string())
    }
}

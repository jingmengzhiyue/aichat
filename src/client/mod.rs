pub mod localai;
pub mod openai;

use self::{
    localai::LocalAIConfig,
    openai::{OpenAIClient, OpenAIConfig},
};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::{ClientBuilder, Proxy};
use serde::Deserialize;
use std::{env, time::Duration};
use tokio::time::sleep;

use crate::{
    client::localai::LocalAIClient,
    config::{Config, SharedConfig},
    repl::{ReplyStreamHandler, SharedAbortSignal},
    utils::split_text,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ClientConfig {
    #[serde(rename = "openai")]
    OpenAI(OpenAIConfig),
    #[serde(rename = "localai")]
    LocalAI(LocalAIConfig),
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub client: String,
    pub name: String,
    pub max_tokens: usize,
    pub index: usize,
}

impl Default for ModelInfo {
    fn default() -> Self {
        OpenAIClient::list_models(&OpenAIConfig::default(), 0)[0].clone()
    }
}

impl ModelInfo {
    pub fn new(client: &str, name: &str, max_tokens: usize, index: usize) -> Self {
        Self {
            client: client.into(),
            name: name.into(),
            max_tokens,
            index,
        }
    }
    pub fn stringify(&self) -> String {
        format!("{}:{}", self.client, self.name)
    }
}

#[async_trait]
pub trait Client {
    fn get_config(&self) -> &SharedConfig;

    fn send_message(&self, content: &str) -> Result<String> {
        init_tokio_runtime()?.block_on(async {
            if self.get_config().read().dry_run {
                return Ok(self.get_config().read().echo_messages(content));
            }
            self.send_message_inner(content)
                .await
                .with_context(|| "Failed to fetch")
        })
    }

    fn send_message_streaming(
        &self,
        content: &str,
        handler: &mut ReplyStreamHandler,
    ) -> Result<()> {
        async fn watch_abort(abort: SharedAbortSignal) {
            loop {
                if abort.aborted() {
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        let abort = handler.get_abort();
        init_tokio_runtime()?.block_on(async {
            tokio::select! {
                ret = async {
                    if self.get_config().read().dry_run {
                        let words = split_text(content)?;
                        for word in words {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            handler.text(&self.get_config().read().echo_messages(&word))?;
                        }
                        return Ok(());
                    }
                    self.send_message_streaming_inner(content, handler).await
                } => {
                    handler.done()?;
                    ret.with_context(|| "Failed to fetch stream")
                }
                _ = watch_abort(abort.clone()) => {
                    handler.done()?;
                    Ok(())
                 },
                _ =  tokio::signal::ctrl_c() => {
                    abort.set_ctrlc();
                    Ok(())
                }
            }
        })
    }

    async fn send_message_inner(&self, content: &str) -> Result<String>;

    async fn send_message_streaming_inner(
        &self,
        content: &str,
        handler: &mut ReplyStreamHandler,
    ) -> Result<()>;
}

pub fn init_client(config: SharedConfig) -> Result<Box<dyn Client>> {
    OpenAIClient::init(config.clone())
        .or_else(|| LocalAIClient::init(config.clone()))
        .ok_or_else(|| {
            let model_info = config.read().model_info.clone();
            anyhow!(
                "Unknown client {} at config.clients[{}]",
                &model_info.client,
                &model_info.index
            )
        })
}

pub fn all_clients() -> Vec<&'static str> {
    vec![OpenAIClient::name(), LocalAIClient::name()]
}

pub fn create_client_config(client: &str) -> Result<String> {
    if client == OpenAIClient::name() {
        OpenAIClient::create_config()
    } else if client == LocalAIClient::name() {
        LocalAIClient::create_config()
    } else {
        bail!("Unknown client {}", &client)
    }
}

pub fn list_models(config: &Config) -> Vec<ModelInfo> {
    config
        .clients
        .iter()
        .enumerate()
        .flat_map(|(i, v)| match v {
            ClientConfig::OpenAI(c) => OpenAIClient::list_models(c, i),
            ClientConfig::LocalAI(c) => LocalAIClient::list_models(c, i),
        })
        .collect()
}

pub fn init_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .with_context(|| "Failed to init tokio")
}

pub(crate) fn set_proxy(builder: ClientBuilder, proxy: &Option<String>) -> Result<ClientBuilder> {
    let proxy = if let Some(proxy) = proxy {
        if proxy.is_empty() || proxy == "false" || proxy == "-" {
            return Ok(builder);
        }
        proxy.clone()
    } else if let Ok(proxy) = env::var("HTTPS_PROXY").or_else(|_| env::var("ALL_PROXY")) {
        proxy
    } else {
        return Ok(builder);
    };
    let builder =
        builder.proxy(Proxy::all(&proxy).with_context(|| format!("Invalid proxy `{proxy}`"))?);
    Ok(builder)
}

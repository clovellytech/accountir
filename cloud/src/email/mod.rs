//! Email delivery via Resend. If RESEND_API_KEY is unset, send() returns an
//! Unconfigured error so callers can fall back to a "copy link" flow.

use serde::Serialize;
use std::env;

#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error("email not configured (set RESEND_API_KEY)")]
    Unconfigured,
    #[error("resend api error: {0}")]
    Api(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Clone)]
pub struct EmailClient {
    api_key: Option<String>,
    from_address: String,
    http: reqwest::Client,
}

impl EmailClient {
    pub fn from_env() -> Self {
        let api_key = env::var("RESEND_API_KEY").ok().filter(|s| !s.is_empty());
        let from_address = env::var("RESEND_FROM")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Maven <onboarding@resend.dev>".to_string());
        Self {
            api_key,
            from_address,
            http: reqwest::Client::new(),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.api_key.is_some()
    }

    pub async fn send(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        reply_to: Option<&str>,
    ) -> Result<(), EmailError> {
        let key = self.api_key.as_deref().ok_or(EmailError::Unconfigured)?;

        #[derive(Serialize)]
        struct ResendReq<'a> {
            from: &'a str,
            to: Vec<&'a str>,
            subject: &'a str,
            html: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_to: Option<&'a str>,
        }

        let body = ResendReq {
            from: &self.from_address,
            to: vec![to],
            subject,
            html,
            reply_to,
        };

        let resp = self
            .http
            .post("https://api.resend.com/emails")
            .bearer_auth(key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(EmailError::Api(format!("{status}: {text}")));
        }
        Ok(())
    }
}

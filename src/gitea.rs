use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;

#[derive(Debug, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoteRepository {
    pub name: String,
    #[serde(default)]
    pub ssh_url: String,
    pub clone_url: String,
    #[serde(default)]
    pub object_format_name: String,
}

#[derive(Debug)]
pub enum Lookup<T> {
    Found(T),
    Missing,
}

pub struct GiteaClient {
    client: Client,
    api_url: Url,
}

impl GiteaClient {
    pub fn new(instance_url: &str, token: &str, allow_insecure_http: bool) -> Result<Self> {
        let root = format!("{}/", instance_url.trim_end_matches('/'));
        let root = Url::parse(&root).context("invalid Gitea URL")?;
        match root.scheme() {
            "https" => {}
            "http" if allow_insecure_http => {}
            "http" => bail!("Gitea URL uses insecure HTTP; pass --allow-insecure-http to allow it"),
            scheme => bail!("unsupported Gitea URL scheme: {scheme}"),
        }
        let api_url = root
            .join("api/v1/")
            .context("failed to construct Gitea API URL")?;

        let mut authorization = HeaderValue::from_str(&format!("token {token}"))
            .context("Gitea token contains invalid characters")?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let client = Client::builder()
            .default_headers(headers)
            .redirect(Policy::none())
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .context("failed to initialize HTTP client")?;
        Ok(Self { client, api_url })
    }

    pub fn current_user(&self) -> Result<User> {
        self.get_required(self.endpoint(&["user"])?)
    }

    pub fn organization(&self, name: &str) -> Result<Lookup<()>> {
        let response = self
            .client
            .get(self.endpoint(&["orgs", name])?)
            .send()
            .context("Gitea API request failed")?;
        match response.status() {
            StatusCode::OK => Ok(Lookup::Found(())),
            StatusCode::NOT_FOUND => Ok(Lookup::Missing),
            _ => {
                let status = response.status();
                let message = response_message(response);
                bail!("query Gitea failed with {status}: {message}")
            }
        }
    }

    pub fn create_organization(&self, name: &str, public: bool) -> Result<()> {
        #[derive(Serialize)]
        struct CreateOrganization<'a> {
            username: &'a str,
            visibility: &'static str,
        }

        let response = self
            .client
            .post(self.endpoint(&["orgs"])?)
            .json(&CreateOrganization {
                username: name,
                visibility: if public { "public" } else { "private" },
            })
            .send()
            .context("failed to create organization")?;
        expect_success(response, StatusCode::CREATED, "create organization")?;
        Ok(())
    }

    pub fn repository(&self, owner: &str, name: &str) -> Result<Lookup<RemoteRepository>> {
        self.get_optional(self.endpoint(&["repos", owner, name])?)
    }

    pub fn create_repository(
        &self,
        owner: &str,
        name: &str,
        public: bool,
        object_format: &str,
    ) -> Result<RemoteRepository> {
        #[derive(Serialize)]
        struct CreateRepository<'a> {
            name: &'a str,
            private: bool,
            auto_init: bool,
            object_format_name: &'a str,
        }

        let response = self
            .client
            .post(self.endpoint(&["orgs", owner, "repos"])?)
            .json(&CreateRepository {
                name,
                private: !public,
                auto_init: false,
                object_format_name: object_format,
            })
            .send()
            .with_context(|| format!("failed to create repository {owner}/{name}"))?;
        let response = expect_success(response, StatusCode::CREATED, "create repository")?;
        response
            .json()
            .context("failed to decode created repository")
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.api_url.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow!("Gitea API URL cannot contain path segments"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    fn get_required<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let response = self
            .client
            .get(url)
            .send()
            .context("Gitea API request failed")?;
        let response = expect_success(response, StatusCode::OK, "query Gitea")?;
        response.json().context("failed to decode Gitea response")
    }

    fn get_optional<T: DeserializeOwned>(&self, url: Url) -> Result<Lookup<T>> {
        let response = self
            .client
            .get(url)
            .send()
            .context("Gitea API request failed")?;
        match response.status() {
            StatusCode::OK => Ok(Lookup::Found(
                response.json().context("failed to decode Gitea response")?,
            )),
            StatusCode::NOT_FOUND => Ok(Lookup::Missing),
            _ => {
                let status = response.status();
                let message = response_message(response);
                bail!("query Gitea failed with {status}: {message}")
            }
        }
    }
}

fn expect_success(response: Response, expected: StatusCode, action: &str) -> Result<Response> {
    if response.status() == expected {
        return Ok(response);
    }
    let status = response.status();
    let message = response_message(response);
    bail!("{action} failed with {status}: {message}")
}

fn response_message(response: Response) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        message: Option<String>,
    }

    let text = response.text().unwrap_or_default();
    if let Ok(error) = serde_json::from_str::<ApiError>(&text)
        && let Some(message) = error.message
    {
        return message;
    }
    let truncated = text.chars().take(500).collect::<String>();
    if truncated.is_empty() {
        "empty response".to_owned()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_preserves_instance_subpath_and_escapes_names() {
        let client = GiteaClient::new("https://example.com/code", "secret", false).unwrap();

        let endpoint = client.endpoint(&["repos", "my org", "repo/name"]).unwrap();

        assert_eq!(
            endpoint.as_str(),
            "https://example.com/code/api/v1/repos/my%20org/repo%2Fname"
        );
    }

    #[test]
    fn insecure_http_requires_explicit_permission() {
        let error = GiteaClient::new("http://example.com", "secret", false)
            .err()
            .unwrap();

        assert!(error.to_string().contains("insecure HTTP"));
    }
}

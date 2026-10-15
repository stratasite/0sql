//! How a refusal is reported, the same whether it was made here or by
//! a server: `class: message` on stderr, or `{"error": {"class",
//! "message"}}` on stdout under `--json`, and exit code 1 either way.
//! A usage error is clap's and exits 2.

use serde_json::json;

/// A refusal with a class, as the API answers one and as the service
/// raises one. Any other error is reported as what it says.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub class: String,
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class, self.message)
    }
}

impl std::error::Error for Refusal {}

impl From<zsql_service::ServiceError> for Refusal {
    fn from(e: zsql_service::ServiceError) -> Self {
        Refusal { class: e.class.to_string(), message: e.message }
    }
}

/// The class and message of any error: a refusal's own, else `Error`
/// and the error's chain.
pub fn parts(e: &anyhow::Error) -> (String, String) {
    match e.downcast_ref::<Refusal>() {
        Some(r) => (r.class.clone(), r.message.clone()),
        None => ("Error".into(), format!("{e:#}")),
    }
}

/// Reports the error the way the command's output mode asks for.
pub fn report(e: &anyhow::Error, json: bool) {
    let (class, message) = parts(e);
    if json {
        println!("{}", json!({ "error": { "class": class, "message": message } }));
    } else if class == "Error" {
        eprintln!("error: {message}");
    } else {
        eprintln!("{class}: {message}");
    }
}

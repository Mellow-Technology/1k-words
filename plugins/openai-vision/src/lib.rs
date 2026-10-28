//! A detection plugin that asks a vision model behind an OpenAI-compatible
//! chat completions API what's in each photo: OpenAI itself, services that
//! copy its API (such as OpenRouter), or a server on your own computer or
//! network, such as Ollama or LM Studio, so photos stay there.
//!
//! The model is sent the photo's thumbnail and asked to list what's in it as
//! JSON. Models differ in how they say where things are, so boxes are taken
//! as fractions of the photo, its pixels or thousandths of it, whichever
//! fits, and dropped if they make no sense.

use exports::onekwords::plugins::detect::{
    Bounds, DetectError, DetectedObject, Guest, Image, ProviderInfo,
};
use onekwords::plugins::http::{self, Request, Response};
use onekwords::plugins::settings::{Config, ConfigField, FieldKind};
use serde_json::{Value, json};

wit_bindgen::generate!({
    path: "../../wit",
    world: "detect-plugin",
});

const PROMPT: &str = "\
List the distinct objects you can see in the photo: people, animals, \
vehicles, food, furniture, plants, and other things someone might search \
their photos for. Name each with a short, common, lowercase noun in the \
singular, such as \"dog\", \"bicycle\" or \"birthday cake\". List each kind \
of thing once for every one you can see, up to 20 objects in all.

Answer with JSON only, in this form:
{\"objects\": [{\"label\": \"dog\", \"confidence\": 0.9, \"box\": [0.1, 0.2, 0.5, 0.9]}]}

\"confidence\" is how sure you are, from 0 to 1. \"box\" is where the object \
is: its left, top, right and bottom edges as fractions of the photo's width \
and height, from its top left corner. Answer {\"objects\": []} if there's \
nothing to list.";

struct OpenAiVision;

impl Guest for OpenAiVision {
    fn info() -> ProviderInfo {
        let field = |key: &str, label: &str, kind, required, default: Option<&str>, help: &str| {
            ConfigField {
                key: key.into(),
                label: label.into(),
                kind,
                required,
                default: default.map(Into::into),
                help: Some(help.into()).filter(|help: &String| !help.is_empty()),
            }
        };
        ProviderInfo {
            id: "openai-vision".into(),
            name: "OpenAI-Compatible Vision Model".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Asks a vision model what's in each photo, through an OpenAI-compatible \
                API: OpenAI, OpenRouter, or a server such as Ollama or LM Studio. Each photo's \
                thumbnail is sent to the server you choose."
                .into(),
            config: vec![
                field(
                    "endpoint",
                    "API URL",
                    FieldKind::Url,
                    true,
                    Some("https://api.openai.com/v1"),
                    "For Ollama, http://localhost:11434/v1; for LM Studio, http://localhost:1234/v1.",
                ),
                field(
                    "model",
                    "Model",
                    FieldKind::Text,
                    true,
                    None,
                    "One that can see images, such as gpt-4.1-mini, or qwen2.5vl with Ollama.",
                ),
                field(
                    "api_key",
                    "API Key",
                    FieldKind::Secret,
                    false,
                    None,
                    "Not needed for most servers on your own computer.",
                ),
            ],
            hosts: Vec::new(),
        }
    }

    fn validate(config: Config) -> Result<(), DetectError> {
        let api = Api::new(&config)?;
        let response = api.send("GET", "models", None)?;
        match response.status {
            200..=299 => {}
            // Not every server lists its models.
            404 | 405 => return Ok(()),
            _ => return Err(error_from(&response)),
        }
        let listed: Vec<String> = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|body| {
                let models = body.get("data")?.as_array()?;
                Some(
                    models
                        .iter()
                        .filter_map(|model| Some(model.get("id")?.as_str()?.to_owned()))
                        .collect(),
                )
            })
            .unwrap_or_default();
        // Ollama names models with a tag, as in "llava:latest".
        let known = listed.is_empty()
            || listed
                .iter()
                .any(|id| id == api.model || id.strip_suffix(":latest") == Some(api.model));
        if !known {
            return Err(DetectError::Config(format!(
                "The server has no model named \"{}\".",
                api.model
            )));
        }
        Ok(())
    }

    fn detect(config: Config, image: Image) -> Result<Vec<DetectedObject>, DetectError> {
        let api = Api::new(&config)?;
        let photo = format!("data:image/jpeg;base64,{}", base64(&image.jpeg));
        let request = json!({
            "model": api.model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": PROMPT},
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": photo}},
                ]},
            ],
        });
        let response = api.send("POST", "chat/completions", Some(&request))?;
        if !(200..300).contains(&response.status) {
            return Err(error_from(&response));
        }
        let answer = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|body| {
                let message = body.get("choices")?.get(0)?.get("message")?;
                Some(message.get("content")?.as_str()?.to_owned())
            })
            .ok_or_else(|| {
                DetectError::Transient("The server's answer wasn't understood.".into())
            })?;
        parse_objects(&answer, image.width, image.height)
            .ok_or_else(|| DetectError::Rejected("The model didn't list the objects.".into()))
    }
}

export!(OpenAiVision);

/// The API, from the settings.
struct Api<'a> {
    /// Without a trailing `/`.
    endpoint: &'a str,
    model: &'a str,
    api_key: Option<&'a str>,
}

impl<'a> Api<'a> {
    fn new(config: &'a Config) -> Result<Self, DetectError> {
        let get = |key: &str| {
            config
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.trim())
                .filter(|value| !value.is_empty())
        };
        let endpoint = get("endpoint")
            .ok_or_else(|| DetectError::Config("API URL is required.".into()))?
            .trim_end_matches('/');
        if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
            return Err(DetectError::Config(
                "The API URL must start with https:// or http://.".into(),
            ));
        }
        Ok(Self {
            endpoint,
            model: get("model").ok_or_else(|| DetectError::Config("Model is required.".into()))?,
            api_key: get("api_key"),
        })
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Response, DetectError> {
        let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
        if let Some(key) = self.api_key {
            headers.push(("authorization".to_owned(), format!("Bearer {key}")));
        }
        let body = match body {
            Some(body) => {
                headers.push(("content-type".to_owned(), "application/json".to_owned()));
                body.to_string().into_bytes()
            }
            None => Vec::new(),
        };
        let request = Request {
            method: method.to_owned(),
            url: format!("{}/{path}", self.endpoint),
            headers,
        };
        http::send(&request, &body).map_err(DetectError::Transient)
    }
}

/// The objects in the model's answer, most certain first; none if it
/// isn't a list of them. The JSON may be wrapped in other text, such as a
/// Markdown code block.
fn parse_objects(answer: &str, width: u32, height: u32) -> Option<Vec<DetectedObject>> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let answer: Value = serde_json::from_str(answer.get(start..=end)?).ok()?;
    let mut objects: Vec<DetectedObject> = answer
        .get("objects")?
        .as_array()?
        .iter()
        .filter_map(|object| {
            let label = object.get("label")?.as_str()?.trim().to_lowercase();
            if label.is_empty() {
                return None;
            }
            let confidence = object
                .get("confidence")
                .and_then(Value::as_f64)
                .map_or(0.8, |confidence| confidence.clamp(0., 1.))
                as f32;
            let bounds = object.get("box").and_then(|b| parse_box(b, width, height));
            Some(DetectedObject {
                label,
                confidence,
                bounds,
            })
        })
        .collect();
    objects.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    Some(objects)
}

/// A box, as left, top, right and bottom: fractions of the photo, its
/// pixels, or thousandths of it, whichever fits.
fn parse_box(value: &Value, width: u32, height: u32) -> Option<Bounds> {
    let numbers: Vec<f32> = value
        .as_array()?
        .iter()
        .map(|n| n.as_f64().map(|n| n as f32))
        .collect::<Option<_>>()?;
    let [left, top, right, bottom] = numbers[..] else {
        return None;
    };
    let largest = numbers.iter().copied().fold(0., f32::max);
    let (x_scale, y_scale) = if largest <= 1. {
        (1., 1.)
    } else if right <= width as f32 && bottom <= height as f32 {
        (width as f32, height as f32)
    } else if largest <= 1000. {
        (1000., 1000.)
    } else {
        return None;
    };
    let bounds = Bounds {
        left: (left / x_scale).clamp(0., 1.),
        top: (top / y_scale).clamp(0., 1.),
        right: (right / x_scale).clamp(0., 1.),
        bottom: (bottom / y_scale).clamp(0., 1.),
    };
    (bounds.right > bounds.left && bounds.bottom > bounds.top).then_some(bounds)
}

/// Classifies a failed response by its status.
fn error_from(response: &Response) -> DetectError {
    let message = serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|body| {
            let error = body.get("error")?;
            let message = error.get("message").or(Some(error))?;
            Some(message.as_str()?.to_owned())
        })
        .unwrap_or_else(|| format!("HTTP {}", response.status));
    match response.status {
        401 | 403 => DetectError::Config(format!("The API key wasn't accepted: {message}")),
        // A wrong URL or model, or a model that can't see images.
        400 | 404 | 422 => DetectError::Config(message),
        408 | 429 | 500..=599 => DetectError::Transient(message),
        _ => DetectError::Transient(message),
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (ix, &byte)| n | u32::from(byte) << (16 - 8 * ix));
        for ix in 0..4 {
            if ix <= chunk.len() {
                encoded.push(ALPHABET[(n >> (18 - 6 * ix) & 63) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn reads_objects_and_their_boxes() {
        let answer = "```json\n{\"objects\": [
            {\"label\": \"Cat\", \"confidence\": 0.5, \"box\": [0.1, 0.2, 0.3, 0.4]},
            {\"label\": \"dog\", \"confidence\": 0.9, \"box\": [56, 37, 280, 186]},
            {\"label\": \"tree\", \"box\": [100, 200, 900, 1000]},
            {\"label\": \"cloud\", \"box\": [0.5, 0.5, 0.2, 0.2]},
            {\"label\": \" \"}
        ]}\n```";
        let objects = parse_objects(answer, 560, 373).unwrap();
        let labels: Vec<&str> = objects.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["dog", "tree", "cloud", "cat"]);
        // Pixels.
        let dog = objects[0].bounds.unwrap();
        assert!((dog.left - 0.1).abs() < 1e-3 && (dog.right - 0.5).abs() < 1e-3);
        // Thousandths, being beyond the photo's pixels.
        let tree = objects[1].bounds.unwrap();
        assert!((tree.top - 0.2).abs() < 1e-3 && (tree.bottom - 1.).abs() < 1e-3);
        assert_eq!(objects[1].confidence, 0.8);
        // Backwards: no box.
        assert!(objects[2].bounds.is_none());
        assert!(parse_objects("I can't see anything.", 1, 1).is_none());
    }
}

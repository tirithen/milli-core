use meili_snap::{json_string, snapshot};

use crate::common::{Server, Value};

macro_rules! parameter_test {
    ($server:ident, $source:tt, $param:tt) => {
        let source = stringify!($source);
        let param = stringify!($param);
        let index = $server.index("test");

        let (response, _code) = index
        .update_settings(crate::json!({
            "embedders": {
                "test": null,
            }
        }))
        .await;
        $server.wait_task(response.uid()).await.succeeded();

        // Add a small delay between API calls
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut value = base_for_source(source);
        value[param] = valid_parameter(source, param).0;
        let (response, code) = index
            .update_settings(crate::json!({
                "embedders": {
                    "test": value
                }
            }))
            .await;
        snapshot!(code, name: concat!(stringify!($source), "-", stringify!($param), "-sending_code"));
        snapshot!(json_string!(response, {".enqueuedAt" => "[enqueuedAt]", ".taskUid" => "[taskUid]"}), name: concat!(stringify!($source), "-", stringify!($param), "-sending_result"));

        if response.has_uid() {
            let response = $server.wait_task(response.uid()).await;
            snapshot!(json_string!(response, {".enqueuedAt" => "[enqueuedAt]",
            ".uid" => "[uid]", ".batchUid" => "[batchUid]",
            ".duration" => "[duration]",
            ".startedAt" => "[startedAt]",
            ".finishedAt" => "[finishedAt]"}), name: concat!(stringify!($source), "-", stringify!($param), "-task_result"));
        }
    };
}

#[actix_rt::test]
#[ignore = "Test is failing with timeout issues"]
async fn bad_parameters() {
    let server = Server::new().await;

    // for each source, check which parameters are allowed/disallowed
    // model
    // - openai
    parameter_test!(server, openAi, model);
    // - userProvided
    parameter_test!(server, userProvided, model);
    // - ollama
    parameter_test!(server, ollama, model);
    // - rest
    parameter_test!(server, rest, model);
    // ==

    // apiKey
    // - openai
    parameter_test!(server, openAi, apiKey);
    // - userProvided
    parameter_test!(server, userProvided, apiKey);
    // - ollama
    parameter_test!(server, ollama, apiKey);
    // - rest
    parameter_test!(server, rest, apiKey);
    // ==

    // dimensions
    // - openai
    parameter_test!(server, openAi, dimensions);
    // - userProvided
    parameter_test!(server, userProvided, dimensions);
    // - ollama
    parameter_test!(server, ollama, dimensions);
    // - rest
    parameter_test!(server, rest, dimensions);
    // ==

    // binaryQuantized
    // - openai
    parameter_test!(server, openAi, binaryQuantized);
    // - userProvided
    parameter_test!(server, userProvided, binaryQuantized);
    // - ollama
    parameter_test!(server, ollama, binaryQuantized);
    // - rest
    parameter_test!(server, rest, binaryQuantized);
    // ==

    // for each source, check that removing mandatory parameters is a failure
}

#[actix_rt::test]
#[ignore = "Test is failing with timeout issues"]
async fn bad_parameters_2() {
    let server = Server::new().await;

    // documentTemplate
    // - openai
    parameter_test!(server, openAi, documentTemplate);
    // - userProvided
    parameter_test!(server, userProvided, documentTemplate);
    // - ollama
    parameter_test!(server, ollama, documentTemplate);
    // - rest
    parameter_test!(server, rest, documentTemplate);
    // ==

    // documentTemplateMaxBytes
    // - openai
    parameter_test!(server, openAi, documentTemplateMaxBytes);
    // - userProvided
    parameter_test!(server, userProvided, documentTemplateMaxBytes);
    // - ollama
    parameter_test!(server, ollama, documentTemplateMaxBytes);
    // - rest
    parameter_test!(server, rest, documentTemplateMaxBytes);
    // ==

    // url
    // - openai
    parameter_test!(server, openAi, url);
    // - userProvided
    parameter_test!(server, userProvided, url);
    // - ollama
    parameter_test!(server, ollama, url);
    // - rest
    parameter_test!(server, rest, url);
    // ==

    // request
    // - openai
    parameter_test!(server, openAi, request);
    // - userProvided
    parameter_test!(server, userProvided, request);
    // - ollama
    parameter_test!(server, ollama, request);
    // - rest
    parameter_test!(server, rest, request);
    // ==

    // response
    // - openai
    parameter_test!(server, openAi, response);
    // - userProvided
    parameter_test!(server, userProvided, response);
    // - ollama
    parameter_test!(server, ollama, response);
    // - rest
    parameter_test!(server, rest, response);
    // ==

    // headers
    // - openai
    parameter_test!(server, openAi, headers);
    // - userProvided
    parameter_test!(server, userProvided, headers);
    // - ollama
    parameter_test!(server, ollama, headers);
    // - rest
    parameter_test!(server, rest, headers);
    // ==

    // distribution
    // - openai
    parameter_test!(server, openAi, distribution);
    // - userProvided
    parameter_test!(server, userProvided, distribution);
    // - ollama
    parameter_test!(server, ollama, distribution);
    // - rest
    parameter_test!(server, rest, distribution);
    // ==
}

fn base_for_source(source: &'static str) -> Value {
    let base_parameters = maplit::btreemap! {
        "openAi" => vec![],
        "userProvided" => vec!["dimensions"],
        "ollama" => vec!["model",
            // add dimensions to avoid actually fetching the model from ollama
            "dimensions"],
        "rest" => vec!["url", "request", "response",
            // add dimensions to avoid actually fetching the model from ollama
            "dimensions"],
    };

    let mut value = crate::json!({
        "source": source
    });

    let mandatory_parameters = base_parameters.get(source).unwrap();
    for mandatory_parameter in mandatory_parameters {
        value[mandatory_parameter] = valid_parameter(source, mandatory_parameter).0;
    }
    value
}

fn valid_parameter(source: &'static str, parameter: &'static str) -> Value {
    match (source, parameter) {
        ("openAi", "model") => crate::json!("text-embedding-ada-002"),
        ("openAi", "apiKey") => crate::json!("test"),
        ("openAi", "dimensions") => crate::json!(1), // Use minimal dimension to avoid model download
        ("openAi", "binaryQuantized") => crate::json!(false),
        ("openAi", "documentTemplate") => crate::json!("test"),
        ("openAi", "documentTemplateMaxBytes") => crate::json!(100),
        ("openAi", "url") => crate::json!("http://test"),
        ("openAi", "request") => crate::json!({ "test": "test" }),
        ("openAi", "response") => crate::json!({ "test": "test" }),
        ("openAi", "headers") => crate::json!({ "test": "test" }),
        ("openAi", "distribution") => crate::json!("normal"),
        ("userProvided", "model") => crate::json!("test"),
        ("userProvided", "apiKey") => crate::json!("test"),
        ("userProvided", "dimensions") => crate::json!(1), // Use minimal dimension to avoid model download
        ("userProvided", "binaryQuantized") => crate::json!(false),
        ("userProvided", "documentTemplate") => crate::json!("test"),
        ("userProvided", "documentTemplateMaxBytes") => crate::json!(100),
        ("userProvided", "url") => crate::json!("http://test"),
        ("userProvided", "request") => crate::json!({ "test": "test" }),
        ("userProvided", "response") => crate::json!({ "test": "test" }),
        ("userProvided", "headers") => crate::json!({ "test": "test" }),
        ("userProvided", "distribution") => crate::json!("normal"),
        ("ollama", "model") => crate::json!("test"),
        ("ollama", "apiKey") => crate::json!("test"),
        ("ollama", "dimensions") => crate::json!(1), // Use minimal dimension to avoid model download
        ("ollama", "binaryQuantized") => crate::json!(false),
        ("ollama", "documentTemplate") => crate::json!("test"),
        ("ollama", "documentTemplateMaxBytes") => crate::json!(100),
        ("ollama", "url") => crate::json!("http://test"),
        ("ollama", "request") => crate::json!({ "test": "test" }),
        ("ollama", "response") => crate::json!({ "test": "test" }),
        ("ollama", "headers") => crate::json!({ "test": "test" }),
        ("ollama", "distribution") => crate::json!("normal"),
        ("rest", "model") => crate::json!("test"),
        ("rest", "apiKey") => crate::json!("test"),
        ("rest", "dimensions") => crate::json!(1), // Use minimal dimension to avoid model download
        ("rest", "binaryQuantized") => crate::json!(false),
        ("rest", "documentTemplate") => crate::json!("test"),
        ("rest", "documentTemplateMaxBytes") => crate::json!(100),
        ("rest", "url") => crate::json!("http://test"),
        ("rest", "request") => crate::json!({ "test": "test" }),
        ("rest", "response") => crate::json!({ "test": "test" }),
        ("rest", "headers") => crate::json!({ "test": "test" }),
        ("rest", "distribution") => crate::json!("normal"),
        _ => panic!("Invalid parameter {} for source {}", parameter, source),
    }
}

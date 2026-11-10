use serde_json::Value;

use super::*;

#[test]
fn settings_and_the_editor_schema_agree_on_every_fixture() {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../../schema/ganja-cloud-substrate.schema.json"
    ))
    .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid settings schema");
    let corpus: Value =
        serde_json::from_str(include_str!("settings_cases.json")).expect("settings fixtures");
    for case in corpus["cases"].as_array().expect("case array") {
        let mut document = corpus["base"].clone();
        for (key, value) in case["patch"].as_object().expect("patch object") {
            if value.is_null() {
                document.as_object_mut().expect("document object").remove(key);
            } else {
                document[key] = value.clone();
            }
        }
        let name = case["name"].as_str().expect("case name");
        let expected = case["valid"].as_bool().expect("expected classification");
        let text = toml::to_string(&document).expect("fixture TOML");
        assert_eq!(Settings::parse(&text).is_ok(), expected, "loader: {name}");
        assert_eq!(validator.is_valid(&document), expected, "schema: {name}");
    }
}

#[test]
fn omitted_settings_have_the_documented_runtime_defaults() {
    let settings = Settings::parse("kubeconfig = '/operator/config'\ntemplate = 'worker'\ngit_credential_uri = 'ate-secret://kubernetes/ganja-cloud/git/token'\n").expect("minimal settings");
    assert_eq!(settings.atespace, "ganja");
    assert_eq!(settings.service_account, "ganja");
    assert_eq!(settings.api_service, "api");
    assert_eq!(settings.router_service, "atenet-router");
    assert_eq!(settings.tunnel, TunnelMode::InProcess);
    assert!(settings.worker_selector.is_empty());
    assert!(settings.git_credential_prefix.is_empty());
    assert!(settings.context.is_none());
    assert!(settings.kubectl.is_none());
    assert!(!format!("{settings:?}").contains("/operator/config"));
}

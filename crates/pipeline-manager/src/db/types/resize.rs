use feldera_types::config::ResourceConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;

/// New CPU and memory values for a running pipeline. Fields left out keep their value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineResize {
    pub cpu_cores_min: Option<f64>,
    pub cpu_cores_max: Option<f64>,
    pub memory_mb_min: Option<u64>,
    pub memory_mb_max: Option<u64>,
}

/// Applies `resize` to a deployment configuration and returns the new one.
pub fn resize_deployment_config(
    deployment_config: &Value,
    resize: &PipelineResize,
) -> Result<Value, String> {
    if resize == &PipelineResize::default() {
        return Err("no resource to resize".to_string());
    }
    if let Some(hosts) = deployment_config.get("hosts").and_then(Value::as_u64)
        && hosts > 1
    {
        return Err(format!("a pipeline with {hosts} hosts cannot be resized"));
    }
    let current = resources_of(deployment_config)?;
    let mut new = current.clone();
    new.cpu_cores_min = check_cpu("cpu_cores_min", current.cpu_cores_min, resize.cpu_cores_min)?;
    new.cpu_cores_max = check_cpu("cpu_cores_max", current.cpu_cores_max, resize.cpu_cores_max)?;
    new.memory_mb_min = check_memory("memory_mb_min", current.memory_mb_min, resize.memory_mb_min)?;
    new.memory_mb_max = check_memory("memory_mb_max", current.memory_mb_max, resize.memory_mb_max)?;
    if let (Some(min), Some(max)) = (new.cpu_cores_min, new.cpu_cores_max)
        && min > max
    {
        return Err(format!(
            "cpu_cores_min ({min}) exceeds cpu_cores_max ({max})"
        ));
    }
    if let (Some(min), Some(max)) = (new.memory_mb_min, new.memory_mb_max)
        && min > max
    {
        return Err(format!(
            "memory_mb_min ({min}) exceeds memory_mb_max ({max})"
        ));
    }
    if is_guaranteed(&current) != is_guaranteed(&new) {
        return Err(
            "the change would move the pipeline's pod between the Guaranteed and Burstable \
             QoS classes, which Kubernetes does not allow while it runs"
                .to_string(),
        );
    }

    let mut result = deployment_config.clone();
    let resources = result
        .as_object_mut()
        .ok_or("deployment configuration is not a JSON object")?
        .entry("resources")
        .or_insert_with(|| json!({}));
    for (field, value) in [
        ("cpu_cores_min", new.cpu_cores_min.map(|v| json!(v))),
        ("cpu_cores_max", new.cpu_cores_max.map(|v| json!(v))),
        ("memory_mb_min", new.memory_mb_min.map(|v| json!(v))),
        ("memory_mb_max", new.memory_mb_max.map(|v| json!(v))),
    ] {
        resources[field] = value.unwrap_or(Value::Null);
    }
    Ok(result)
}

fn resources_of(config: &Value) -> Result<ResourceConfig, String> {
    match config.get("resources") {
        Some(resources) => ResourceConfig::deserialize(resources)
            .map_err(|e| format!("unable to parse resources: {e}")),
        None => Ok(ResourceConfig::default()),
    }
}

/// Guaranteed QoS: each request equals its limit. An unset CPU request defaults to the limit.
fn is_guaranteed(resources: &ResourceConfig) -> bool {
    let cpu = match (resources.cpu_cores_min, resources.cpu_cores_max) {
        (Some(min), Some(max)) => min == max,
        (None, Some(_)) => true,
        _ => false,
    };
    let memory = matches!(
        (resources.memory_mb_min, resources.memory_mb_max),
        (Some(min), Some(max)) if min == max
    );
    cpu && memory
}

fn check_cpu(name: &str, current: Option<f64>, new: Option<f64>) -> Result<Option<f64>, String> {
    let Some(new) = new else {
        return Ok(current);
    };
    if !new.is_finite() || new <= 0.0 {
        return Err(format!("{name} must be greater than 0"));
    }
    match current {
        None => Err(format!("{name} is not set, so it cannot be resized")),
        Some(_) => Ok(Some(new)),
    }
}

fn check_memory(name: &str, current: Option<u64>, new: Option<u64>) -> Result<Option<u64>, String> {
    let Some(new) = new else {
        return Ok(current);
    };
    if new == 0 {
        return Err(format!("{name} must be greater than 0"));
    }
    match current {
        None => Err(format!("{name} is not set, so it cannot be resized")),
        Some(_) => Ok(Some(new)),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn deployment() -> Value {
        json!({
            "workers": 4,
            "name": "pipeline-x",
            "inputs": {},
            "resources": {
                "cpu_cores_min": 4.0, "cpu_cores_max": 8.0,
                "memory_mb_min": 16000, "memory_mb_max": 16000,
                "storage_mb_max": 1000, "namespace": null
            }
        })
    }

    fn resize(value: Value) -> PipelineResize {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn resize_changes_only_the_given_fields() {
        let new = resize_deployment_config(
            &deployment(),
            &resize(json!({"cpu_cores_min": 1.0, "memory_mb_min": 4000})),
        )
        .unwrap();
        let mut expected = deployment();
        expected["resources"]["cpu_cores_min"] = json!(1.0);
        expected["resources"]["memory_mb_min"] = json!(4000);
        assert_eq!(new, expected);
    }

    #[test]
    fn increases_are_allowed() {
        let new = resize_deployment_config(
            &deployment(),
            &resize(json!({"cpu_cores_max": 16.0, "memory_mb_min": 20000, "memory_mb_max": 24000})),
        )
        .unwrap();
        assert_eq!(new["resources"]["cpu_cores_max"], json!(16.0));
        assert_eq!(new["resources"]["memory_mb_max"], json!(24000));
    }

    fn assert_refused(value: Value, expected: &str) {
        let error = resize_deployment_config(&deployment(), &resize(value.clone())).unwrap_err();
        assert!(error.contains(expected), "{value}: {error}");
    }

    #[test]
    fn invalid_resizes_are_refused() {
        assert_refused(json!({}), "no resource");
        assert_refused(json!({"cpu_cores_min": 0.0}), "greater than 0");
        assert_refused(json!({"memory_mb_max": 0}), "greater than 0");
        assert_refused(json!({"memory_mb_min": 17000}), "exceeds");
        assert_refused(json!({"cpu_cores_min": 9.0}), "exceeds");
        // Every request equal to its limit would make the pod Guaranteed.
        assert_refused(json!({"cpu_cores_max": 4.0}), "QoS");
        let mut multihost = deployment();
        multihost["hosts"] = json!(2);
        let error = resize_deployment_config(&multihost, &resize(json!({"cpu_cores_min": 1.0})))
            .unwrap_err();
        assert!(error.contains("2 hosts"), "{error}");
        let error = serde_json::from_value::<PipelineResize>(json!({"workers": 2})).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn unset_field_cannot_be_resized() {
        let mut config = deployment();
        config["resources"]["cpu_cores_min"] = json!(null);
        let error =
            resize_deployment_config(&config, &resize(json!({"cpu_cores_min": 1.0}))).unwrap_err();
        assert!(error.contains("not set"), "{error}");
    }

    #[test]
    fn guaranteed_pod_resizes_request_and_limit_together() {
        let mut config = deployment();
        config["resources"]["cpu_cores_max"] = json!(4.0);
        // Lowering only the memory request would make it Burstable.
        let error =
            resize_deployment_config(&config, &resize(json!({"memory_mb_min": 8000}))).unwrap_err();
        assert!(error.contains("QoS"), "{error}");
        assert!(
            resize_deployment_config(
                &config,
                &resize(json!({"memory_mb_min": 8000, "memory_mb_max": 8000}))
            )
            .is_ok()
        );
    }
}

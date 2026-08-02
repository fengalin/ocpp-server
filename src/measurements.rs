#[derive(Debug, serde::Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SampledValue {
    pub context: String,
    pub format: String,
    pub location: String,
    pub measurand: String,
    pub phase: String,
    pub unit: String,
    pub value: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Eq, PartialEq)]
pub struct Dpm {
    #[serde(rename = "DPM")]
    pub data: DpmData,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DpmData {
    pub sampled_value: Vec<SampledValue>,
    pub timestamp: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<i32>,
}

#[derive(Debug)]
#[expect(unused)]
pub struct DataTransfer {
    pub timestamp: String,
    pub transaction_id: Option<i32>,
    pub sampled_values: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_dpm_data() {
        // let dpm_data = "[{\"DPM\":{\"sampledValue\":[{\"context\":\"Sample.Clock\",\"format\":\"Raw\",\"location\":\"Inlet\",\"measurand\":\"Voltage\",\"phase\":\"L1\",\"unit\":\"V\",\"value\":\"233\"}],\"timestamp\":\"2026-08-02T17:46:24Z\"}}]";
        let dpm_data = "[{\"DPM\":{\"sampledValue\":[{\"context\":\"Sample.Clock\",\"format\":\"Raw\",\"location\":\"Inlet\",\"measurand\":\"Voltage\",\"phase\":\"L1\",\"unit\":\"V\",\"value\":\"234\"},{\"context\":\"Sample.Clock\",\"format\":\"Raw\",\"location\":\"Inlet\",\"measurand\":\"Voltage\",\"phase\":\"L2\",\"unit\":\"V\",\"value\":\"0\"},{\"context\":\"Sample.Clock\",\"format\":\"Raw\",\"location\":\"Inlet\",\"measurand\":\"Voltage\",\"phase\":\"L3\",\"unit\":\"V\",\"value\":\"0\"}],\"timestamp\":\"2026-08-02T14:51:15Z\"}}]";
        let dpms = serde_json::from_str::<Vec<Dpm>>(dpm_data).unwrap();
        assert_eq!(
            dpms,
            vec![Dpm {
                data: DpmData {
                    sampled_value: vec![
                        SampledValue {
                            context: "Sample.Clock".to_string(),
                            format: "Raw".to_string(),
                            location: "Inlet".to_string(),
                            measurand: "Voltage".to_string(),
                            phase: "L1".to_string(),
                            unit: "V".to_string(),
                            value: "234".to_string(),
                        },
                        SampledValue {
                            context: "Sample.Clock".to_string(),
                            format: "Raw".to_string(),
                            location: "Inlet".to_string(),
                            measurand: "Voltage".to_string(),
                            phase: "L2".to_string(),
                            unit: "V".to_string(),
                            value: "0".to_string(),
                        },
                        SampledValue {
                            context: "Sample.Clock".to_string(),
                            format: "Raw".to_string(),
                            location: "Inlet".to_string(),
                            measurand: "Voltage".to_string(),
                            phase: "L3".to_string(),
                            unit: "V".to_string(),
                            value: "0".to_string(),
                        }
                    ],
                    timestamp: "2026-08-02T14:51:15Z".to_string(),
                    transaction_id: None,
                },
            }]
        );
    }
}

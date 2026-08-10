#[derive(Debug, serde::Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SampledValue {
    pub context: String,
    pub format: String,
    pub location: String,
    pub measurand: String,
    pub unit: String,
    pub value: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
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
pub struct DataTransfer {
    pub timestamp: String,
    pub transaction_id: Option<i32>,
    pub sampled_values: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_dpm_data_clock() {
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
                            unit: "V".to_string(),
                            value: "234".to_string(),
                            phase: Some("L1".to_string()),
                        },
                        SampledValue {
                            context: "Sample.Clock".to_string(),
                            format: "Raw".to_string(),
                            location: "Inlet".to_string(),
                            measurand: "Voltage".to_string(),
                            unit: "V".to_string(),
                            value: "0".to_string(),
                            phase: Some("L2".to_string()),
                        },
                        SampledValue {
                            context: "Sample.Clock".to_string(),
                            format: "Raw".to_string(),
                            location: "Inlet".to_string(),
                            measurand: "Voltage".to_string(),
                            unit: "V".to_string(),
                            value: "0".to_string(),
                            phase: Some("L3".to_string()),
                        }
                    ],
                    timestamp: "2026-08-02T14:51:15Z".to_string(),
                    transaction_id: None,
                },
            }]
        );
    }

    #[test]
    fn deserialize_dpm_data_periodic() {
        let dpm_data = "[{\"DPM\":{\"sampledValue\":[{\"context\":\"Sample.Periodic\",\"format\":\"Raw\",\"location\":\"Inlet\",\"measurand\":\"Energy.Active.Import.Register\",\"unit\":\"Wh\",\"value\":\"3190200\"},{\"context\":\"Sample.Periodic\",\"format\":\"Raw\",\"location\":\"Inlet\",\"measurand\":\"Power.Active.Import\",\"unit\":\"W\",\"value\":\"7525\"}],\"timestamp\":\"2026-07-31T13:31:58Z\"},\"transactionId\":1}]";
        let dpms = serde_json::from_str::<Vec<Dpm>>(dpm_data).unwrap();
        assert_eq!(
            dpms,
            vec![Dpm {
                data: DpmData {
                    sampled_value: vec![
                        SampledValue {
                            context: "Sample.Periodic".to_string(),
                            format: "Raw".to_string(),
                            location: "Inlet".to_string(),
                            measurand: "Energy.Active.Import.Register".to_string(),
                            unit: "Wh".to_string(),
                            value: "3190200".to_string(),
                            phase: None,
                        },
                        SampledValue {
                            context: "Sample.Periodic".to_string(),
                            format: "Raw".to_string(),
                            location: "Inlet".to_string(),
                            measurand: "Power.Active.Import".to_string(),
                            unit: "W".to_string(),
                            value: "7525".to_string(),
                            phase: None,
                        },
                    ],
                    timestamp: "2026-07-31T13:31:58Z".to_string(),
                    transaction_id: None,
                },
            }]
        );
    }
}

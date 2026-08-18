use chrono::{DateTime, Local, Utc};
use ocpp_rs::v16::{data_types as v16_data_types, enums as v16_enums};
use std::{cmp, fmt};

#[derive(Debug, Default, Clone)]
pub struct MeterValueSelection {
    pub timestamp: DateTime<Utc>,
    pub active_energy_import: Option<u64>,
    pub active_power_import: Option<u64>,
    pub voltage_l1: Option<u64>,
    pub temperature: Option<u64>,
}

impl<'a> From<&'a v16_data_types::MeterValue> for MeterValueSelection {
    fn from(mv: &'a v16_data_types::MeterValue) -> Self {
        let mut this = MeterValueSelection {
            timestamp: mv.timestamp.inner(),
            ..Default::default()
        };

        for mv in mv.sampled_value.iter() {
            use v16_enums::Measurand::*;
            match mv.measurand {
                Some(EnergyActiveImportRegister) => {
                    this.active_energy_import = mv.value.parse::<u64>().ok()
                }
                Some(PowerActiveImport) => this.active_power_import = mv.value.parse::<u64>().ok(),
                Some(Voltage) if mv.phase == Some(v16_enums::Phase::L1) => {
                    this.voltage_l1 = mv.value.parse::<u64>().ok()
                }
                Some(Temperature) => this.temperature = mv.value.parse::<u64>().ok(),
                _ => (),
            }
        }

        this
    }
}

impl fmt::Display for MeterValueSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "ts: {}, energy: {:.3} kWh, power: {:.3} kW, L1: {:.0} V, temp.: {:.0} °C",
            self.timestamp.with_timezone(&Local),
            self.active_energy_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.active_power_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.voltage_l1.map_or(f64::NAN, |v| v as f64),
            self.temperature.map_or(f64::NAN, |v| v as f64),
        ))
    }
}

impl cmp::PartialEq for MeterValueSelection {
    #[allow(clippy::eq_op)]
    fn eq(&self, other: &Self) -> bool {
        self.active_energy_import == other.active_energy_import
            && self.active_energy_import == other.active_energy_import
            && Option::zip(self.voltage_l1, other.voltage_l1)
                .is_some_and(|(s, o)| u64::abs_diff(s, o) < 3)
            && self.temperature == other.temperature
    }
}

#[derive(Debug, Default)]
pub struct DpmSelection {
    pub timestamp: String,
    pub transaction_id: Option<i32>,
    pub active_energy_import: Option<u64>,
    pub active_power_import: Option<u64>,
    pub voltage_l1: Option<u64>,
}

impl From<Dpm> for DpmSelection {
    fn from(dpm: Dpm) -> Self {
        let mut this = DpmSelection {
            timestamp: dpm.data.timestamp.clone(),
            transaction_id: dpm.data.transaction_id,
            ..Default::default()
        };

        for sv in dpm.data.sampled_value.iter() {
            match sv.measurand.as_str() {
                "Energy.Active.Import.Register" => {
                    this.active_energy_import = sv.value.parse::<u64>().ok()
                }
                "Power.Active.Import" => this.active_power_import = sv.value.parse::<u64>().ok(),
                "Voltage" if sv.phase.as_deref() == Some("L1") => {
                    this.voltage_l1 = sv.value.parse::<u64>().ok()
                }
                _ => (),
            }
        }

        this
    }
}

impl fmt::Display for DpmSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "ts: {}, energy: {:.3} kWh, power: {:.3} kW, L1: {:.0} V",
            self.timestamp,
            self.active_energy_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.active_power_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.voltage_l1.map_or(f64::NAN, |v| v as f64),
        ))
    }
}

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

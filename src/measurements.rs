use chrono::{DateTime, Local, Utc};
use ocpp_rs::v16::{data_types as v16_data_types, enums as v16_enums};
use std::{cmp, fmt};

/// Voltage floor step from which to report a difference when computing Eq
const VOLTAGE_DIFF_STEP: u64 = 3;
/// Voltage offset to add when computing Eq, so the steps are centered around 230 V
const VOLTAGE_DIFF_OFFSET: u64 = 2;

/// DPM energy rounding step from which to report a difference when computing Eq
const DPM_ENERGY_DIFF_STEP: u64 = 1_000;
/// DPM power rounding step from which to report a difference when computing Eq
const DPM_POWER_DIFF_STEP: u64 = 500;

fn voltage_eq(lhs: u64, rhs: u64) -> bool {
    ((lhs + VOLTAGE_DIFF_OFFSET) / VOLTAGE_DIFF_STEP)
        == ((rhs + VOLTAGE_DIFF_OFFSET) / VOLTAGE_DIFF_STEP)
}

#[derive(Debug, Default, Clone)]
pub struct MeterValueSelection {
    pub timestamp: DateTime<Utc>,
    pub transaction_id: Option<i32>,
    pub active_energy_import: Option<u64>,
    pub active_power_import: Option<u64>,
    pub voltage_l1: Option<u64>,
    pub temperature: Option<u64>,
}

impl MeterValueSelection {
    pub fn new(mv: &v16_data_types::MeterValue, transaction_id: Option<i32>) -> Self {
        let mut this = MeterValueSelection {
            timestamp: mv.timestamp.inner(),
            transaction_id,
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
            "ts: {}, energy: {:.3} kWh, power: {:.3} kW, L1: {:.0} V, temp.: {:.0} °C, tid: {:?}",
            self.timestamp.with_timezone(&Local),
            self.active_energy_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.active_power_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.voltage_l1.map_or(f64::NAN, |v| v as f64),
            self.temperature.map_or(f64::NAN, |v| v as f64),
            self.transaction_id,
        ))
    }
}

impl cmp::PartialEq for MeterValueSelection {
    #[allow(clippy::eq_op)]
    fn eq(&self, other: &Self) -> bool {
        self.active_energy_import == other.active_energy_import
            && Option::zip(self.active_power_import, other.active_power_import)
                .is_none_or(|(s, o)| s == o)
            && Option::zip(self.voltage_l1, other.voltage_l1).is_none_or(|(s, o)| voltage_eq(s, o))
    }
}

/// Meter Value Temperature observer
/// Avoids logging when temperature fluctuates between two values
#[derive(Debug, Default, Clone)]
pub struct MeterValueObserver {
    prev_meter_val: Option<MeterValueSelection>,
    prev_temp_mean: Option<f64>,
    last_voltage: Option<u64>,
}

impl MeterValueObserver {
    /// Consolidates successive meter values
    ///
    /// Some MeterValues come with voltage but no power measure,
    /// and they are usually followed by another MeterValue set
    /// containing power but no voltages.
    ///
    /// This function keep track of voltage when present and
    /// adds it to the next MeterValue set, so only the
    /// consolidated one will be selected for log.
    pub fn consolidate(&mut self, mv: &mut MeterValueSelection) {
        match (mv.voltage_l1, mv.active_power_import) {
            (Some(voltage), None) => self.last_voltage = Some(voltage),
            (None, Some(_power)) => {
                mv.voltage_l1 = self.last_voltage.take();
            }
            _ => (),
        }
    }

    pub fn pertinent_to_user(&mut self, mv: &MeterValueSelection) -> bool {
        if mv.active_power_import.is_none() {
            // see consolidate()
            return false;
        }

        let Some(prev_mv) = self.prev_meter_val.take() else {
            self.prev_meter_val = Some(mv.clone());
            self.prev_temp_mean = None;
            return true;
        };

        self.prev_meter_val = Some(mv.clone());
        let temp_mean = Option::zip(prev_mv.temperature, mv.temperature)
            .map(|(prev_temp, temp)| (prev_temp as f64).midpoint(temp as f64));

        let prev_temp_mean = std::mem::replace(&mut self.prev_temp_mean, temp_mean);

        if prev_mv == *mv {
            if let Some(prev_temp) = prev_mv.temperature
                && let Some(temp) = mv.temperature
                && prev_temp == temp
            {
                // all the same
                false
            } else if let Some(prev_temp_mean) = prev_temp_mean
                && let Some(temp_mean) = temp_mean
                && prev_temp_mean == temp_mean
            {
                // all the same
                false
            } else {
                // evolving temperature or undefined / defined
                true
            }
        } else {
            // different values
            true
        }
    }
}

#[derive(Debug, Default)]
pub struct DpmSelection {
    pub timestamp: Option<DateTime<Utc>>,
    pub active_energy_import: Option<u64>,
    pub active_power_import: Option<u64>,
}

impl From<Dpm> for DpmSelection {
    fn from(dpm: Dpm) -> Self {
        let mut this = DpmSelection {
            timestamp: dpm.data.timestamp.parse().ok(),
            ..Default::default()
        };

        for sv in dpm.data.sampled_value.iter() {
            match sv.measurand.as_str() {
                "Energy.Active.Import.Register" => {
                    this.active_energy_import = sv.value.parse::<u64>().ok()
                }
                "Power.Active.Import" => this.active_power_import = sv.value.parse::<u64>().ok(),
                _ => (),
            }
        }

        this
    }
}

fn dpm_energy_eq(lhs: u64, rhs: u64) -> bool {
    ((lhs + DPM_ENERGY_DIFF_STEP / 2) / DPM_ENERGY_DIFF_STEP)
        == ((rhs + DPM_ENERGY_DIFF_STEP / 2) / DPM_ENERGY_DIFF_STEP)
}

fn dpm_power_eq(lhs: u64, rhs: u64) -> bool {
    ((lhs + DPM_POWER_DIFF_STEP / 2) / DPM_POWER_DIFF_STEP)
        == ((rhs + DPM_POWER_DIFF_STEP / 2) / DPM_POWER_DIFF_STEP)
}

impl cmp::PartialEq for DpmSelection {
    #[allow(clippy::eq_op)]
    fn eq(&self, other: &Self) -> bool {
        Option::zip(self.active_energy_import, other.active_energy_import)
            .is_none_or(|(s, o)| dpm_energy_eq(s, o))
            && Option::zip(self.active_power_import, other.active_power_import)
                .is_none_or(|(s, o)| dpm_power_eq(s, o))
    }
}

impl fmt::Display for DpmSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "ts: {}, energy: {:.3} kWh, power: {:.3} kW",
            self.timestamp.map_or_else(
                || "invalid".to_string(),
                |ts| ts.with_timezone(&Local).to_string()
            ),
            self.active_energy_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.active_power_import
                .map_or(f64::NAN, |v| v as f64 / 1_000.0),
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

    #[test]
    fn measure_eq() {
        assert!(dpm_energy_eq(10, 490));
        assert!(dpm_energy_eq(510, 990));
        assert!(dpm_energy_eq(510, 1_040));
        assert!(!dpm_energy_eq(490, 510));
        assert!(!dpm_energy_eq(510, 1_510));

        assert!(dpm_power_eq(10, 240));
        assert!(dpm_power_eq(250, 490));
        assert!(dpm_power_eq(500, 740));
        assert!(!dpm_power_eq(240, 490));
        assert!(!dpm_power_eq(500, 760));

        assert!(voltage_eq(229, 230));
        assert!(voltage_eq(231, 230));
        assert!(!voltage_eq(228, 230));
        assert!(!voltage_eq(229, 232));
        assert!(!voltage_eq(230, 232));
    }

    #[test]
    fn meter_values_eq() {
        let now = Utc::now();

        // exact match
        assert_eq!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
        );

        // exact match but one is missing power
        assert_eq!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: None,
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
        );

        // timestamps are excluded from comparison
        assert_eq!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
            MeterValueSelection {
                timestamp: Utc::now(),
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
        );

        // temperature are excluded from comparison
        assert_eq!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
            MeterValueSelection {
                timestamp: Utc::now(),
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(27),
                transaction_id: Some(1),
            },
        );

        // transction_id are excluded from comparison
        assert_eq!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: None,
            },
            MeterValueSelection {
                timestamp: Utc::now(),
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(27),
                transaction_id: Some(1),
            },
        );

        // different energy
        assert_ne!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_100),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
        );

        // different power
        assert_ne!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(110),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
        );

        // different voltage step
        assert_ne!(
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(230),
                temperature: Some(26),
                transaction_id: Some(1),
            },
            MeterValueSelection {
                timestamp: now,
                active_energy_import: Some(1_000),
                active_power_import: Some(100),
                voltage_l1: Some(232),
                temperature: Some(26),
                transaction_id: Some(1),
            },
        );
    }

    #[test]
    fn metervalue_observer() {
        let mut mvo = MeterValueObserver::default();

        // initial
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_000),
            active_power_import: Some(100),
            voltage_l1: Some(230),
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // different energy
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_100),
            active_power_import: Some(100),
            voltage_l1: Some(230),
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // different power
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_100),
            active_power_import: Some(110),
            voltage_l1: Some(230),
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // same as previous (no log)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_100),
            active_power_import: Some(110),
            voltage_l1: None,
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // voltage, but no active power (no log, but consolidation on next MV)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: None,
            voltage_l1: Some(230),
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // no voltage, but active power (consolidated voltage from MV and log)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: Some(10),
            voltage_l1: None,
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert_eq!(Some(230), mv.voltage_l1);
        assert!(mvo.pertinent_to_user(&mv));

        // different voltage step, no active power (no log, but consolidation on next MV)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: None,
            voltage_l1: Some(232),
            temperature: None,
            transaction_id: None,
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // no voltage, but active power (consolidated voltage from MV and log)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: Some(10),
            voltage_l1: None,
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert_eq!(Some(232), mv.voltage_l1);
        assert!(mvo.pertinent_to_user(&mv));

        // same as previous, different temperature (log)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: Some(110),
            voltage_l1: Some(232),
            temperature: Some(27),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // same as previous, fluctuating temperature (no log)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: Some(110),
            voltage_l1: None,
            temperature: Some(26),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // same as previous, fluctuating temperature again (no log)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: Some(110),
            voltage_l1: None,
            temperature: Some(27),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // same as previous, raising temperature (log)
        let mut mv = MeterValueSelection {
            timestamp: Utc::now(),
            active_energy_import: Some(1_110),
            active_power_import: Some(110),
            voltage_l1: None,
            temperature: Some(28),
            transaction_id: Some(1),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));
    }

    #[test]
    fn dpm_selection_eq() {
        // Eq
        assert_eq!(
            DpmSelection {
                timestamp: Some(Utc::now()),
                active_energy_import: Some(1_500),
                active_power_import: Some(240),
            },
            DpmSelection {
                timestamp: Some(Utc::now()),
                active_energy_import: Some(1_550),
                active_power_import: Some(50),
            },
        );

        assert_eq!(
            DpmSelection {
                timestamp: Some(Utc::now()),
                active_energy_import: Some(1_500),
                active_power_import: Some(510),
            },
            DpmSelection {
                timestamp: None,
                active_energy_import: Some(1_990),
                active_power_import: Some(740),
            },
        );

        // !Eq energy
        assert_ne!(
            DpmSelection {
                timestamp: Some(Utc::now()),
                active_energy_import: Some(1_500),
                active_power_import: Some(730),
            },
            DpmSelection {
                timestamp: None,
                active_energy_import: Some(2_500),
                active_power_import: Some(730),
            },
        );

        // !Eq power
        assert_ne!(
            DpmSelection {
                timestamp: Some(Utc::now()),
                active_energy_import: Some(1_500),
                active_power_import: Some(740),
            },
            DpmSelection {
                timestamp: None,
                active_energy_import: Some(1_500),
                active_power_import: Some(760),
            },
        );
    }
}

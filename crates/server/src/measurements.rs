use chrono::{DateTime, Local, Utc};
use ocpp_rs::v16::{data_types as v16_data_types, enums as v16_enums};
use std::{cmp, fmt};

/// Voltage floor step from which to report a difference when computing Eq
const VOLTAGE_DIFF_STEP: u16 = 3;
/// Voltage offset to add when computing Eq, so the steps are centered around 230 V
const VOLTAGE_DIFF_OFFSET: u16 = 2;

/// DPM energy rounding step from which to report a difference when computing Eq
const DPM_ENERGY_DIFF_STEP: u64 = 1_000;
/// DPM power rounding step from which to report a difference when computing Eq
const DPM_POWER_DIFF_STEP: u32 = 500;

fn voltage_eq(lhs: u16, rhs: u16) -> bool {
    ((lhs + VOLTAGE_DIFF_OFFSET) / VOLTAGE_DIFF_STEP)
        == ((rhs + VOLTAGE_DIFF_OFFSET) / VOLTAGE_DIFF_STEP)
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct MeterValueSelection {
    pub ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voltage: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<u16>,
}

impl MeterValueSelection {
    pub fn is_informative(&self) -> bool {
        self.power.is_some()
            || self.energy.is_some()
            || self.voltage.is_some()
            || self.temperature.is_some()
    }
}

impl<'a> From<&'a v16_data_types::MeterValue> for MeterValueSelection {
    fn from(mv: &v16_data_types::MeterValue) -> Self {
        let mut this = MeterValueSelection {
            ts: mv.timestamp.inner(),
            ..Default::default()
        };

        for mv in mv.sampled_value.iter() {
            use v16_enums::Measurand::*;
            match mv.measurand {
                Some(EnergyActiveImportRegister) => this.energy = mv.value.parse::<u64>().ok(),
                Some(PowerActiveImport) => this.power = mv.value.parse::<u32>().ok(),
                Some(Voltage) if mv.phase == Some(v16_enums::Phase::L1) => {
                    this.voltage = mv.value.parse::<u16>().ok()
                }
                Some(Temperature) => this.temperature = mv.value.parse::<u16>().ok(),
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
            self.ts.with_timezone(&Local),
            self.energy.map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.power.map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.voltage.map_or(f64::NAN, |v| v as f64),
            self.temperature.map_or(f64::NAN, |v| v as f64),
        ))
    }
}

impl cmp::PartialEq for MeterValueSelection {
    #[allow(clippy::eq_op)]
    fn eq(&self, other: &Self) -> bool {
        self.energy == other.energy
            && Option::zip(self.power, other.power).is_none_or(|(s, o)| s == o)
            && Option::zip(self.voltage, other.voltage).is_none_or(|(s, o)| voltage_eq(s, o))
    }
}

/// Meter Value Temperature observer
/// Avoids logging when temperature fluctuates between two values
#[derive(Debug, Default, Clone)]
pub struct MeterValueObserver {
    prev_meter_val: Option<MeterValueSelection>,
    prev_temp_mean: Option<f32>,
    last_voltage: Option<u16>,
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
        match (mv.voltage, mv.power) {
            (Some(voltage), None) => self.last_voltage = Some(voltage),
            (None, Some(_power)) => {
                mv.voltage = self.last_voltage.take();
            }
            _ => (),
        }
    }

    pub fn pertinent_to_user(&mut self, mv: &MeterValueSelection) -> bool {
        let Some(prev_mv) = self.prev_meter_val.take() else {
            self.prev_meter_val = Some(mv.clone());
            self.prev_temp_mean = None;
            return true;
        };

        self.prev_meter_val = Some(mv.clone());
        let temp_mean = Option::zip(prev_mv.temperature, mv.temperature)
            .map(|(prev_temp, temp)| (prev_temp as f32).midpoint(temp as f32));

        let prev_temp_mean = std::mem::replace(&mut self.prev_temp_mean, temp_mean);

        // FIXME this doesn't compare temperatures, add the eq code here if it's not needed elsewhere
        if prev_mv == *mv {
            return false;
        }

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
    }
}

#[derive(Debug, Default)]
pub struct DpmSelection {
    pub timestamp: Option<DateTime<Utc>>,
    pub energy: Option<u64>,
    pub power: Option<u32>,
    pub voltage: Option<u16>,
}

impl From<Dpm> for DpmSelection {
    fn from(dpm: Dpm) -> Self {
        let mut this = DpmSelection {
            timestamp: dpm.data.timestamp.parse().ok(),
            ..Default::default()
        };

        for sv in dpm.data.sampled_value.iter() {
            match sv.measurand.as_str() {
                "Energy.Active.Import.Register" => this.energy = sv.value.parse::<u64>().ok(),
                "Power.Active.Import" => this.power = sv.value.parse::<u32>().ok(),
                "Voltage" => this.voltage = sv.value.parse::<u16>().ok(),
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

fn dpm_power_eq(lhs: u32, rhs: u32) -> bool {
    ((lhs + DPM_POWER_DIFF_STEP / 2) / DPM_POWER_DIFF_STEP)
        == ((rhs + DPM_POWER_DIFF_STEP / 2) / DPM_POWER_DIFF_STEP)
}

impl cmp::PartialEq for DpmSelection {
    #[allow(clippy::eq_op)]
    fn eq(&self, other: &Self) -> bool {
        Option::zip(self.energy, other.energy).is_none_or(|(s, o)| dpm_energy_eq(s, o))
            && Option::zip(self.power, other.power).is_none_or(|(s, o)| dpm_power_eq(s, o))
            && Option::zip(self.voltage, other.voltage).is_none_or(|(s, o)| s == o)
    }
}

impl fmt::Display for DpmSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "ts: {}, energy: {:.3} kWh, power: {:.3} kW, voltage: {} V",
            self.timestamp.map_or_else(
                || "invalid".to_string(),
                |ts| ts.with_timezone(&Local).to_string()
            ),
            self.energy.map_or(f64::NAN, |v| v as f64 / 1_000.0),
            self.power.map_or(f32::NAN, |v| v as f32 / 1_000.0),
            self.voltage.unwrap_or_default(),
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

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub enum EnergyTracker {
    Increasing(u64),
    Probation(u64),
    Stationnary(u64),
    #[default]
    Unknown,
}

impl EnergyTracker {
    pub fn have_energy(&mut self, energy: u64) {
        use EnergyTracker::*;
        *self = match self {
            Increasing(v) if *v < energy => Increasing(energy),
            // FIXME add an increasing to stationnary probation
            // state before going back to stationnary?
            Increasing(_) => Stationnary(energy),
            Probation(v) if *v < energy => Increasing(energy),
            Probation(_) => Stationnary(energy),
            Stationnary(v) if *v < energy => Increasing(energy),
            Stationnary(_) => Stationnary(energy),
            Unknown => Probation(energy),
        };
    }

    pub fn current(&self) -> Option<u64> {
        use EnergyTracker::*;
        match self {
            Increasing(v) | Probation(v) | Stationnary(v) => Some(*v),
            Unknown => None,
        }
    }
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
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
        );

        // exact match but one is missing power
        assert_eq!(
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: None,
                voltage: Some(230),
                temperature: Some(26),
            },
        );

        // timestamps are excluded from comparison
        assert_eq!(
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: Utc::now(),
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
        );

        // temperature are excluded from comparison
        assert_eq!(
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: Utc::now(),
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(27),
            },
        );

        // transction_id are excluded from comparison
        assert_eq!(
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: Utc::now(),
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(27),
            },
        );

        // different energy
        assert_ne!(
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: now,
                energy: Some(1_100),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
        );

        // different power
        assert_ne!(
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(110),
                voltage: Some(230),
                temperature: Some(26),
            },
        );

        // different voltage step
        assert_ne!(
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(230),
                temperature: Some(26),
            },
            MeterValueSelection {
                ts: now,
                energy: Some(1_000),
                power: Some(100),
                voltage: Some(232),
                temperature: Some(26),
            },
        );
    }

    #[test]
    fn metervalue_observer() {
        let mut mvo = MeterValueObserver::default();

        // initial
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_000),
            power: Some(100),
            voltage: Some(230),
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // different energy
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_100),
            power: Some(100),
            voltage: Some(230),
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // different power
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_100),
            power: Some(110),
            voltage: Some(230),
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // same as previous (no log)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_100),
            power: Some(110),
            voltage: None,
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // voltage, but no active power (no log, but consolidation on next MV)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: None,
            voltage: Some(230),
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // no voltage, but active power (consolidated voltage from MV and log)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: Some(10),
            voltage: None,
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert_eq!(Some(230), mv.voltage);
        assert!(mvo.pertinent_to_user(&mv));

        // different voltage step, no active power (no log, but consolidation on next MV)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: None,
            voltage: Some(232),
            temperature: None,
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // no voltage, but active power (consolidated voltage from MV and log)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: Some(10),
            voltage: None,
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert_eq!(Some(232), mv.voltage);
        assert!(mvo.pertinent_to_user(&mv));

        // same as previous, different temperature (log)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: Some(110),
            voltage: Some(232),
            temperature: Some(27),
        };
        mvo.consolidate(&mut mv);
        assert!(mvo.pertinent_to_user(&mv));

        // same as previous, fluctuating temperature (no log)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: Some(110),
            voltage: None,
            temperature: Some(26),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // same as previous, fluctuating temperature again (no log)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: Some(110),
            voltage: None,
            temperature: Some(27),
        };
        mvo.consolidate(&mut mv);
        assert!(!mvo.pertinent_to_user(&mv));

        // same as previous, raising temperature (log)
        let mut mv = MeterValueSelection {
            ts: Utc::now(),
            energy: Some(1_110),
            power: Some(110),
            voltage: None,
            temperature: Some(28),
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
                energy: Some(1_500),
                power: Some(240),
                voltage: Some(230),
            },
            DpmSelection {
                timestamp: Some(Utc::now()),
                energy: Some(1_550),
                power: Some(50),
                voltage: Some(230),
            },
        );

        assert_eq!(
            DpmSelection {
                timestamp: Some(Utc::now()),
                energy: Some(1_500),
                power: Some(510),
                voltage: Some(230),
            },
            DpmSelection {
                timestamp: None,
                energy: Some(1_990),
                power: Some(740),
                voltage: None,
            },
        );

        // !Eq energy
        assert_ne!(
            DpmSelection {
                timestamp: Some(Utc::now()),
                energy: Some(1_500),
                power: Some(730),
                voltage: Some(230),
            },
            DpmSelection {
                timestamp: None,
                energy: Some(2_500),
                power: Some(730),
                voltage: Some(230),
            },
        );

        // !Eq power
        assert_ne!(
            DpmSelection {
                timestamp: Some(Utc::now()),
                energy: Some(1_500),
                power: Some(740),
                voltage: Some(230),
            },
            DpmSelection {
                timestamp: None,
                energy: Some(1_500),
                power: Some(760),
                voltage: Some(230),
            },
        );
    }

    #[test]
    fn energy_tracker() {
        use EnergyTracker::*;

        // probation to stationnary
        let mut tracker = EnergyTracker::default();
        assert_eq!(tracker, Unknown);

        let mut energy = 100;
        tracker.have_energy(energy);
        assert_eq!(tracker, Probation(energy));

        tracker.have_energy(energy);
        assert_eq!(tracker, Stationnary(energy));
        tracker.have_energy(energy);
        assert_eq!(tracker, Stationnary(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));

        tracker.have_energy(energy);
        assert_eq!(tracker, Stationnary(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));

        // probation to increasing
        let mut tracker = EnergyTracker::default();
        assert_eq!(tracker, Unknown);

        let mut energy = 100;
        tracker.have_energy(energy);
        assert_eq!(tracker, Probation(energy));

        energy += 10;
        tracker.have_energy(energy);
        assert_eq!(tracker, Increasing(energy));
    }
}

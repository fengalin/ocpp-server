//! The meter-value sample, as a focused subset of the OCPP `MeterValue`
//! structure.
//!
//! Designed for the EVSE: it selects a subset of the clumsy OCPP `MeterValue`
//! (originally for logging, later extended to the web app's per-sample SSE
//! stream and `GET /api/metrics`). It is shared between the server, the WASM
//! web front end, and the external OLED status app (which consumes it over the
//! unix socket). The OCPP `From<ocpp_rs MeterValue>` conversion is behind the
//! `ocpp` feature and stays out of the WASM front end.

use crate::model::Ts;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MeterValueSelection {
    pub ts: Ts,
    // `None` fields are written as JSON `null`, not skipped, so every field is
    // always present in the payload and clients never have to guess a missing
    // one. `default` is belt-and-braces for decoders reading a shorter object.
    #[serde(default)]
    pub energy: Option<u64>,
    #[serde(default)]
    pub power: Option<u32>,
    #[serde(default)]
    pub voltage: Option<u16>,
    #[serde(default)]
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

impl std::fmt::Display for MeterValueSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Absent fields print as `-`, not `NaN`.
        let energy = self.energy.map_or(String::from("-"), |v| {
            format!("{:.3} kWh", v as f64 / 1_000.0)
        });
        let power = self.power.map_or(String::from("-"), |v| {
            format!("{:.3} kW", v as f64 / 1_000.0)
        });
        let voltage = self
            .voltage
            .map_or(String::from("-"), |v| format!("{v:.0} V"));
        let temp = self
            .temperature
            .map_or(String::from("-"), |v| format!("{v:.0} °C"));
        f.write_fmt(format_args!(
            "ts: {}, energy: {}, power: {}, L1: {}, temp.: {}",
            self.ts.to_utc(),
            energy,
            power,
            voltage,
            temp,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: every field must round-trip through JSON (the wire codec
    /// of `GET /api/metrics` and the SSE stream) whether present or absent —
    /// an earlier MessagePack iteration failed catch-up as soon as any field
    /// was `None`.
    #[test]
    fn json_roundtrip_with_none_fields() {
        // The shape `GET /api/metrics` puts on the wire: a list of samples.
        let samples = vec![
            MeterValueSelection {
                ts: Ts(1_700_000_000_000),
                energy: Some(1_000),
                power: None,
                voltage: Some(230),
                temperature: Some(26),
            },
            // All `None` (previously: "invalid length 1").
            MeterValueSelection::default(),
        ];
        let bytes = serde_json::to_vec(&samples).expect("serializes");
        let back: Vec<MeterValueSelection> = serde_json::from_slice(&bytes).expect("decodes");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].ts, Ts(1_700_000_000_000));
        assert_eq!(back[0].energy, Some(1_000));
        assert_eq!(back[0].power, None);
        assert_eq!(back[0].voltage, Some(230));
        assert_eq!(back[0].temperature, Some(26));
        assert_eq!(back[1].energy, None);
        assert_eq!(back[1].power, None);
        assert_eq!(back[1].voltage, None);
        assert_eq!(back[1].temperature, None);
    }
}

// ---------------------------------------------------------------------
// OCPP-specific impls (only compiled when the `ocpp` feature is on, i.e.
// on the server — never in the WASM front end or the OLED app).
// ---------------------------------------------------------------------
#[cfg(feature = "ocpp")]
pub mod ocpp_impls {
    use super::MeterValueSelection;
    use crate::model::Ts;
    use ocpp_rs::v16::{data_types as v16_data_types, enums as v16_enums};
    use std::cmp;

    /// Voltage floor step from which to report a difference when computing Eq
    const VOLTAGE_DIFF_STEP: u16 = 3;
    /// Voltage offset to add when computing Eq, so the steps are centered around 230 V
    const VOLTAGE_DIFF_OFFSET: u16 = 2;

    pub fn voltage_eq(lhs: u16, rhs: u16) -> bool {
        ((lhs + VOLTAGE_DIFF_OFFSET) / VOLTAGE_DIFF_STEP)
            == ((rhs + VOLTAGE_DIFF_OFFSET) / VOLTAGE_DIFF_STEP)
    }

    impl From<&v16_data_types::MeterValue> for MeterValueSelection {
        fn from(mv: &v16_data_types::MeterValue) -> Self {
            let mut this = MeterValueSelection {
                ts: Ts::from_utc(mv.timestamp.inner()),
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

    impl cmp::PartialEq for MeterValueSelection {
        #[allow(clippy::eq_op)]
        fn eq(&self, other: &Self) -> bool {
            self.energy == other.energy
                && Option::zip(self.power, other.power).is_none_or(|(s, o)| s == o)
                && Option::zip(self.voltage, other.voltage).is_none_or(|(s, o)| voltage_eq(s, o))
        }
    }
}

use std::{
    fmt::{self, Write},
    ops,
};
use tracing::{debug, error, info};

// FIXME make those a setting also they are not BMS related
pub const DEFAULT_CAPACITY: u32 = 48_100;
pub const MAX_POWER: u32 = 7_400;
pub const STATIC_LOSS: u32 = 400;
pub const MAX_AVAILABLE_POWER: u32 = MAX_POWER - STATIC_LOSS;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Bms {
    capacity: f32,
    initial_energy: Option<u64>,
    current_energy: Option<u64>,
    initial_soc: Option<f32>,
    current_soc: SoC,
    pub target_soc: Option<f32>,
}

impl Bms {
    pub fn builder(capacity: u32) -> BmsBuilder {
        BmsBuilder(Bms {
            capacity: capacity as f32,
            initial_energy: None,
            current_energy: None,
            initial_soc: None,
            current_soc: SoC::Unknown,
            target_soc: None,
        })
    }

    pub fn capacity(&self) -> f32 {
        self.capacity
    }

    pub fn initial_energy(&self) -> Option<u64> {
        self.initial_energy
    }

    pub fn current_energy(&self) -> Option<u64> {
        self.current_energy
    }

    pub fn added_energy(&self) -> Option<u64> {
        Option::zip(self.initial_energy, self.current_energy).map(|(init, cur)| cur - init)
    }

    pub fn initial_soc(&self) -> Option<f32> {
        self.initial_soc
    }

    pub fn current_soc(&self) -> SoC {
        if let Some(init_soc) = self.initial_soc
            && self.current_soc.is_unknown()
        {
            return SoC::Abs(init_soc);
        }

        self.current_soc
    }

    pub fn sync_current_soc(&mut self, new_cur_soc: SoC) -> Result<(), BmsError> {
        if self.initial_soc.is_none() && new_cur_soc.is_absolute() {
            return Err(BmsError::UnknownInitSoCCantSetAbsoluteCur);
        }

        use SoC::*;
        match (&mut self.current_soc, new_cur_soc) {
            (Abs(cur), Abs(new)) => {
                if *cur < new {
                    *cur = new
                }
            }
            (Rel(cur), Rel(new)) => {
                if *cur < new {
                    *cur = new
                }
            }
            _ => self.current_soc = new_cur_soc,
        }

        Ok(())
    }

    /// Initializes the BMS with snapshot retrieved from DB
    ///
    /// This only makes sense on startup
    #[cfg(feature = "server")]
    pub fn init_from_db(&mut self, init_energy: u64, cur_energy: u64, cur_soc: SoC) {
        self.initial_energy = Some(init_energy);
        self.current_energy = Some(cur_energy);

        let Some(raw_abs_cur_soc) = cur_soc.absolute() else {
            self.initial_soc = None;
            self.current_soc = cur_soc;
            return;
        };

        self.initial_soc = Some(
            (raw_abs_cur_soc - cur_energy.saturating_sub(init_energy) as f32 / self.capacity)
                .clamp(0.0, 1.0),
        );
        self.current_soc = cur_soc;
    }

    /// Updates the BMS initial SoC if different and reflect this on current SoC
    ///
    /// Since it's a reference, the new_init_soc if always absolute.
    ///
    /// Returns `true` this lead to changes.
    // FIXME looks like this should be a Result
    pub fn update_initial_soc(&mut self, init_soc: f32) -> bool {
        if !(0.0..=1.0).contains(&init_soc) {
            debug!(init_soc, "out of range");
            return false;
        }

        // can't be known at better precision than 1%
        let new_init_soc = (init_soc * 100.0).round() / 100.0;
        if self
            .initial_soc
            .is_some_and(|cur_init_soc| (cur_init_soc - new_init_soc).abs() < 0.009)
        {
            debug!(cur_bms = %self, new_init_soc, "not updating BMS");
            return false;
        }

        debug!(cur_bms = %self, "updating BMS");

        self.initial_soc = Some(new_init_soc);

        if let Some(cur_energy) = self.current_energy
            && let Some(init_energy) = self.initial_energy
        {
            self.current_soc = SoC::Abs(
                new_init_soc + cur_energy.saturating_sub(init_energy) as f32 / self.capacity,
            )
            .clamp();
        } else {
            self.current_soc = SoC::Abs(new_init_soc);
        }

        debug!(new_bms = %self, "updated BMS");

        true
    }

    /// Record a new meter energy reading (Wh) and re-estimate `current_soc`
    /// from it (relative to `initial_energy`).
    pub fn update_energy(&mut self, cur_energy: u64) {
        let Some(initial_energy) = self.initial_energy else {
            info!(energy = cur_energy, "initializing BMS energy");

            self.initial_energy = Some(cur_energy);
            self.current_energy = Some(cur_energy);

            self.current_soc = if let Some(init_soc) = self.initial_soc {
                SoC::Abs(init_soc)
            } else {
                SoC::Unknown
            };

            return;
        };

        if let Some(prev_energy) = self.current_energy {
            if cur_energy == prev_energy {
                return;
            } else if prev_energy > cur_energy {
                error!(
                    "can't compute SoC progress: new energy: {:.3} kWh, \
                    is less than previous: {:.3} kWh",
                    cur_energy as f32 / 1_000.0,
                    prev_energy as f32 / 1_000.0,
                );
                return;
            }
        } else if initial_energy > cur_energy {
            error!(
                "can't compute SoC progress: new energy: {:.3} kWh, \
                initial energy: {:.3} kWh",
                cur_energy as f64 / 1_000.0,
                initial_energy as f64 / 1_000.0,
            );
            return;
        } else if cur_energy == initial_energy {
            return;
        }

        let added_energy = cur_energy - initial_energy;

        self.current_energy = Some(cur_energy);

        let added_soc = added_energy as f32 / self.capacity;
        self.current_soc = if let Some(init_soc) = self.initial_soc {
            SoC::Abs(init_soc + added_soc).clamp()
        } else {
            SoC::Rel(added_soc).clamp()
        };

        debug!(cur_soc = %self.current_soc, cur_energy, "updated BMS");
    }

    pub fn soc_progress(&self) -> SoCProgress {
        SoCProgress::from_soc_and_target(self.current_soc, self.target_soc)
    }

    /// Computes the raw energy to add to reach the target SoC,
    /// not including constant power loss
    ///
    /// The target SoC is assumed to relate to weither current SoC (and so the init SoC)
    /// are Absolute or Relative.
    pub fn target_soc_to_energy(&self, target_soc: f32) -> Result<u32, BmsError> {
        let target_soc = target_soc.clamp(0.0, 1.0);

        use SoC::*;
        let Abs(cur_soc) = self.current_soc() else {
            return Err(BmsError::UnknownInitSoCCantComputeEnergy);
        };

        if cur_soc >= target_soc {
            return Err(BmsError::TargetSoCReached);
        }

        Ok((self.capacity * (target_soc - cur_soc)) as u32)
    }

    pub fn target_soc(&self) -> Option<f32> {
        self.target_soc
    }
}

impl fmt::Display for Bms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "cap.: {:.3} kWh, init energy: {} kWh, \
            cur energy: {}, init SoC: {} %, cur SoC: {}",
            self.capacity / 1_000.0,
            self.initial_energy
                .map_or_else(|| '—'.to_string(), |e| format!("{:.3}", e as f64 / 1_000.0)),
            self.current_energy
                .map_or_else(|| '—'.to_string(), |e| format!("{:.3}", e as f64 / 1_000.0)),
            if let Some(init_soc) = self.initial_soc {
                format!("{:.0}", init_soc * 100.0)
            } else {
                '—'.to_string()
            },
            SoCProgress::from_soc_and_target(self.current_soc, self.target_soc),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BmsError {
    #[error("Unknown inital SoC, can't set absolute current SoC")]
    UnknownInitSoCCantSetAbsoluteCur,
    #[error("Unknown initial SoC, can't compute energy to add")]
    UnknownInitSoCCantComputeEnergy,
    #[error("Target SoC already reached")]
    TargetSoCReached,
}

pub struct BmsBuilder(Bms);
impl BmsBuilder {
    pub fn initial_energy(mut self, energy: u64) -> Self {
        self.0.initial_energy = Some(energy);
        self
    }
    pub fn current_energy(mut self, energy: u64) -> Self {
        self.0.current_energy = Some(energy);
        self
    }
    pub fn initial_soc(mut self, soc: f32) -> Self {
        self.0.initial_soc = Some(soc.clamp(0.0, 1.0));
        self
    }
    pub fn current_soc(mut self, soc: SoC) -> Self {
        self.0.current_soc = soc.clamp();
        self
    }
    pub fn target_soc(mut self, target_soc: impl Into<Option<f32>>) -> Self {
        self.0.target_soc = target_soc.into().map(|t| t.clamp(0.0, 1.0));
        self
    }
    pub fn build(self) -> Result<Bms, BmsError> {
        if self.0.initial_soc.is_none() && self.0.current_soc.is_absolute() {
            return Err(BmsError::UnknownInitSoCCantSetAbsoluteCur);
        }
        Ok(self.0)
    }
}

/// State of charge: a *fraction* 0–1 (see the crate docs). `PartialEq` is
/// exact (the old epsilon-0.01 impl was non-transitive and leaked into
/// `Bms`'s derived `PartialEq`).
#[derive(Debug, Default, Copy, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum SoC {
    #[default]
    Unknown,
    Abs(f32),
    Rel(f32),
}

impl SoC {
    pub fn new(is_absolute: bool, soc: impl Into<Option<f32>>) -> Self {
        if is_absolute {
            Self::new_absolute(soc).clamp()
        } else {
            Self::new_relative(soc).clamp()
        }
    }

    pub fn new_absolute(soc: impl Into<Option<f32>>) -> Self {
        match soc.into() {
            Some(soc) => SoC::Abs(soc).clamp(),
            None => SoC::Unknown,
        }
    }

    pub fn new_relative(soc: impl Into<Option<f32>>) -> Self {
        match soc.into() {
            Some(soc) => SoC::Rel(soc).clamp(),
            None => SoC::Unknown,
        }
    }

    pub fn is_absolute(self) -> bool {
        matches!(self, SoC::Abs(_))
    }

    pub fn is_relative(self) -> bool {
        matches!(self, SoC::Rel(_))
    }

    pub fn is_unknown(self) -> bool {
        matches!(self, SoC::Unknown)
    }

    pub fn absolute(self) -> Option<f32> {
        self.is_absolute().then(|| self.inner()).flatten()
    }

    pub fn relative(self) -> Option<f32> {
        self.is_relative().then(|| self.inner()).flatten()
    }

    pub fn round_per_100(self) -> SoC {
        fn round(soc: f32) -> f32 {
            (soc * 100.0).round() / 100.0
        }
        use SoC::*;
        match self {
            Abs(soc) => Abs(round(soc)),
            Rel(soc) => Rel(round(soc)),
            Unknown => Unknown,
        }
    }

    pub fn round_per_1000(self) -> SoC {
        fn round(soc: f32) -> f32 {
            (soc * 1000.0).round() / 1000.0
        }
        use SoC::*;
        match self {
            Abs(soc) => Abs(round(soc)),
            Rel(soc) => Rel(round(soc)),
            Unknown => Unknown,
        }
    }

    pub fn clamp(self) -> SoC {
        fn clamp(soc: f32) -> f32 {
            soc.clamp(0.0, 100.0)
        }
        use SoC::*;
        match self {
            Abs(soc) => Abs(clamp(soc)),
            Rel(soc) => Rel(clamp(soc)),
            Unknown => Unknown,
        }
    }

    /// The inner fraction, for either kind (`None` when `Unknown`).
    pub fn inner(self) -> Option<f32> {
        match self {
            SoC::Abs(soc) => Some(soc),
            SoC::Rel(rel_soc) => Some(rel_soc),
            SoC::Unknown => None,
        }
    }

    pub fn pratically_equals(&self, other: SoC) -> bool {
        fn practically_equals(s: f32, o: f32) -> bool {
            (s - o).abs() < 0.001
        }

        use SoC::*;
        match (*self, other) {
            (Abs(s), Abs(o)) => practically_equals(s, o),
            (Rel(s), Rel(o)) => practically_equals(s, o),
            (Unknown, Unknown) => true,
            _ => false,
        }
    }
}

impl ops::Add<f32> for SoC {
    type Output = SoC;
    fn add(mut self, added_soc: f32) -> Self::Output {
        match &mut self {
            SoC::Abs(soc) => *soc += added_soc,
            SoC::Rel(rel_soc) => *rel_soc += added_soc,
            SoC::Unknown => self = SoC::Rel(added_soc),
        }
        self.clamp()
    }
}

impl ops::Sub<f32> for SoC {
    type Output = SoC;
    fn sub(mut self, soc_delta: f32) -> Self::Output {
        match &mut self {
            SoC::Abs(soc) => *soc -= soc_delta,
            SoC::Rel(rel_soc) => *rel_soc -= soc_delta,
            SoC::Unknown => self = SoC::Rel(soc_delta),
        }
        self.clamp()
    }
}

impl fmt::Display for SoC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn fmt_soc(f: &mut fmt::Formatter<'_>, soc: f32) -> fmt::Result {
            f.write_fmt(format_args!("{:.1}", soc * 100.0))
        }
        match self {
            SoC::Abs(soc) => fmt_soc(f, *soc),
            SoC::Rel(soc) => {
                f.write_char('+')?;
                fmt_soc(f, *soc)
            }
            SoC::Unknown => f.write_str("—"),
        }
    }
}

#[derive(Debug, Copy, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub enum SoCProgress {
    Unknown,
    AbsoluteUncapped(f32),
    AbsoluteCap(f32),
    AbsoluteTargetNotReached { soc: f32, target: f32 },
    AbsoluteTargetReached { soc: f32, target: f32 },
    RelativeUncapped(f32),
    RelativeCap(f32),
    RelativeTargetNotReached { rel_soc: f32, target: f32 },
    RelativeCapReached { rel_soc: f32, cap: f32 },
}

impl SoCProgress {
    pub fn new(
        is_absolute: bool,
        soc: impl Into<Option<f32>>,
        target: impl Into<Option<f32>>,
    ) -> Self {
        let soc = soc.into().map(|soc| soc.clamp(0.0, 1.0));
        let cap = target.into().map(|cap| cap.clamp(0.0, 1.0));

        use SoCProgress::*;
        if is_absolute {
            match (soc, cap) {
                (Some(soc), Some(cap)) if soc < cap => {
                    AbsoluteTargetNotReached { soc, target: cap }
                }
                (Some(soc), Some(cap)) => AbsoluteTargetReached { soc, target: cap },
                (Some(soc), None) => AbsoluteUncapped(soc),
                (None, Some(cap)) => AbsoluteCap(cap),
                (None, None) => Unknown,
            }
        } else {
            match (soc, cap) {
                (Some(rel_soc), Some(cap)) if rel_soc < cap => RelativeTargetNotReached {
                    rel_soc,
                    target: cap,
                },
                (Some(rel_soc), Some(cap)) => RelativeCapReached { rel_soc, cap },
                (Some(rel_soc), None) => RelativeUncapped(rel_soc),
                (None, Some(cap)) => RelativeCap(cap),
                (None, None) => Unknown,
            }
        }
    }

    pub fn from_soc_and_target(soc: SoC, target: impl Into<Option<f32>>) -> Self {
        Self::new(soc.is_absolute(), soc.inner(), target)
    }

    pub fn is_complete(self) -> bool {
        use SoCProgress::*;
        matches!(
            self,
            AbsoluteTargetReached { .. } | RelativeCapReached { .. }
        )
    }

    pub fn soc(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(soc) => SoC::Abs(soc),
            AbsoluteCap(_) => SoC::Unknown,
            AbsoluteTargetNotReached { soc, .. } => SoC::Abs(soc),
            AbsoluteTargetReached { soc, .. } => SoC::Abs(soc),
            RelativeUncapped(rel_soc) => SoC::Rel(rel_soc),
            RelativeCap(_) => SoC::Unknown,
            RelativeTargetNotReached { rel_soc, .. } => SoC::Rel(rel_soc),
            RelativeCapReached { rel_soc, .. } => SoC::Rel(rel_soc),
        }
    }

    pub fn target(self) -> SoC {
        use SoCProgress::*;
        match self {
            Unknown => SoC::Unknown,
            AbsoluteUncapped(_) => SoC::Unknown,
            AbsoluteCap(cap) => SoC::Abs(cap),
            AbsoluteTargetNotReached { target: cap, .. } => SoC::Abs(cap),
            AbsoluteTargetReached { target: cap, .. } => SoC::Abs(cap),
            RelativeUncapped(_) => SoC::Unknown,
            RelativeCap(cap) => SoC::Rel(cap),
            RelativeTargetNotReached { target: cap, .. } => SoC::Rel(cap),
            RelativeCapReached { cap, .. } => SoC::Rel(cap),
        }
    }

    pub fn absolute_soc(self) -> Option<f32> {
        self.soc().absolute()
    }
}

impl fmt::Display for SoCProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SoCProgress::*;
        match self {
            Unknown => f.write_str("unknown"),
            AbsoluteUncapped(soc) => write!(f, "uncapped {} %", SoC::Abs(*soc)),
            RelativeUncapped(added_soc) => write!(f, "uncapped {} %", SoC::Rel(*added_soc)),
            RelativeCap(cap) => write!(f, "capped to {} %", SoC::Rel(*cap)),
            _ => {
                write!(f, "{} / {} %", self.soc(), self.target())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soc_progress_constructor() {
        use SoCProgress::*;
        assert_eq!(SoCProgress::new(true, None, None), Unknown);
        assert_eq!(
            SoCProgress::new(true, Some(0.42), None),
            AbsoluteUncapped(0.42)
        );
        assert_eq!(SoCProgress::new(true, None, Some(0.50)), AbsoluteCap(0.50));
        assert_eq!(
            SoCProgress::new(true, Some(0.42), Some(0.50)),
            AbsoluteTargetNotReached {
                soc: 0.42,
                target: 0.50
            }
        );
        assert_eq!(
            SoCProgress::new(true, Some(0.50), Some(0.50)),
            AbsoluteTargetReached {
                soc: 0.50,
                target: 0.50
            }
        );

        assert_eq!(SoCProgress::new(false, None, None), Unknown);
        assert_eq!(
            SoCProgress::new(false, Some(0.42), None),
            RelativeUncapped(0.42)
        );
        assert_eq!(SoCProgress::new(false, None, Some(0.50)), RelativeCap(0.50));
        assert_eq!(
            SoCProgress::new(false, Some(0.42), Some(0.50)),
            RelativeTargetNotReached {
                rel_soc: 0.42,
                target: 0.50
            }
        );
        assert_eq!(
            SoCProgress::new(false, Some(0.50), Some(0.50)),
            RelativeCapReached {
                rel_soc: 0.50,
                cap: 0.50
            }
        );
    }
}

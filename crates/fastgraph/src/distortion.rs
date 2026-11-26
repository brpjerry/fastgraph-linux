//! The Distortion view's data: the analysis of each sweep with a long
//! impulse response, computed once on the worker when the item arrives, and
//! the curves drawn for the measurement the view shows.
use crate::app::{App, Item, Message};
use crate::graph::{Curve, DistortionPlot, Ink, Units};
use crate::theme;
use fastgraph_core::deconv::ImpulseResponse;
use fastgraph_core::distortion::{self, Distortion};
use fastgraph_core::response::Response;
use std::sync::{Arc, Weak};

impl App {
    /// Analyse `item`'s impulse response on the worker, if it is a sweep of
    /// this version, and report it as [`Message::DistortionReady`].
    pub(crate) fn queue_distortion(&self, item: &Item) {
        let meta = &item.m.meta;
        let (Some(ir), Some(_), Some(seconds)) = (&item.ir, meta.ir_peak, meta.sweep_seconds)
        else {
            return;
        };
        let (ir, id) = (Arc::clone(ir), meta.id.clone());
        self.worker.submit(Box::new(move || {
            distortion::analyse(&ir, seconds)
                .map(|d| Message::DistortionReady(id, Arc::downgrade(&ir), Box::new(d)))
        }));
    }

    /// The analysis of `source` arrived, for an item still in the session
    /// with that impulse response (not another that reuses the id).
    pub(crate) fn distortion_ready(
        &mut self,
        id: String,
        source: &Weak<ImpulseResponse>,
        d: Distortion,
    ) {
        let current = self.items.iter().any(|i| {
            i.m.meta.id == id
                && i.ir
                    .as_ref()
                    .is_some_and(|ir| source.ptr_eq(&Arc::downgrade(ir)))
        });
        if current {
            self.distortion.insert(id.clone(), d);
            // Only the displayed item, or the empty-state note, can change.
            if self.graph_view == crate::graph::View::Distortion
                && self
                    .distortion_item()
                    .is_none_or(|item| item.m.meta.id == id)
            {
                self.redraw();
            }
        }
    }

    /// The measurement the Distortion view shows: the selected one if it
    /// has data, and with no selection the first shown one with data.
    fn distortion_item(&self) -> Option<&Item> {
        let has = |i: &&Item| self.distortion.contains_key(&i.m.meta.id);
        match self.selected_item() {
            Some(i) => Some(i).filter(has),
            None => self.items.iter().filter(has).find(|i| i.shown),
        }
    }

    fn selected_item(&self) -> Option<&Item> {
        let id = self.selected.as_ref()?;
        self.items.iter().find(|i| i.m.meta.id == *id)
    }

    /// What the Distortion view says when it has nothing to draw.
    pub(crate) fn distortion_note(&self) -> String {
        if let Some(i) = self.selected_item() {
            format!("{} has no distortion data", i.m.meta.name)
        } else if self.items.is_empty() {
            "No measurements".into()
        } else if self
            .items
            .iter()
            .any(|i| self.distortion.contains_key(&i.m.meta.id))
        {
            "Show a sweep measured with this version".into()
        } else {
            crate::graph::NO_DISTORTION.into()
        }
    }

    /// The curves of the Distortion view, smoothed as the response is.
    /// Compensation, Align and the offset do not apply: they shift levels,
    /// and every curve here is read against the fundamental.
    pub(crate) fn distortion_plot(&self) -> Option<DistortionPlot> {
        let item = self.distortion_item()?;
        let d = self.distortion.get(&item.m.meta.id)?;
        let smoothing = self.prefs.smoothing;
        let units = self.units;
        let fundamental = d.fundamental.smoothed(smoothing);
        let curve = |label: &str, ink: Ink, level: &Response| {
            let mut smoothed = level.smoothed(smoothing);
            // Smoothing averages the valid cells beside a gap; the gap stays one.
            for (value, source) in smoothed.db.iter_mut().zip(&level.db) {
                if source.is_nan() {
                    *value = f64::NAN;
                }
            }
            Curve::new(label, ink, &smoothed, &fundamental, units)
        };
        let mut curves = Vec::new();
        if units == Units::Db {
            curves.push(curve("Fundamental", Ink::Trace(item.color), &d.fundamental));
        }
        // H2.. take the trace colours other than the fundamental's, once each; the
        // eighth harmonic is drawn dim.
        let mut colours = (1..=theme::TRACE_COLOURS)
            .map(|i| i % theme::TRACE_COLOURS)
            .filter(|&i| i != item.color % theme::TRACE_COLOURS)
            .map(Ink::Trace);
        for (k, h) in &d.harmonics {
            let mut c = curve(&format!("H{k}"), colours.next().unwrap_or(Ink::Dim), h);
            c.labelled = true;
            curves.push(c);
        }
        if !d.harmonics.is_empty() {
            let mut thd = curve("THD", Ink::Fg, &d.thd());
            thd.thick = true;
            thd.labelled = true;
            curves.push(thd);
        }
        if let Some(noise) = &d.noise {
            let mut c = curve("Noise", Ink::Dim, noise);
            c.dashed = true;
            curves.push(c);
        }
        Some(DistortionPlot {
            name: item.m.meta.name.clone(),
            units,
            curves,
        })
    }
}

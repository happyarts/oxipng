use indexmap::IndexSet;
use rayon::prelude::*;

use super::PngImage;
#[cfg(not(feature = "parallel"))]
use crate::rayon;
use crate::{
    deflate,
    filters::{FilterStrategy, RowFilter},
};

/// Filters are chosen for sections of about this many bytes.
/// With the context, a trial is about one of libdeflate's blocks (at most 300 000 bytes).
const SECTION_SIZE: usize = 256 * 1024;
/// Each section is compressed after the deflate window of data already chosen.
const WINDOW_SIZE: usize = 32 * 1024;

/// Filters chosen section by section from several strategies, and how each strategy did
#[derive(Debug)]
pub struct SectionFilters {
    /// The filter for each line
    pub filters: Vec<RowFilter>,
    /// The number of sections
    pub sections: usize,
    /// For each strategy, how many bytes of the image it won
    pub won: Vec<usize>,
    /// For each strategy, the filter it chose for each line
    pub strategy_filters: Vec<Vec<RowFilter>>,
    /// For each strategy, its compressed size summed over the sections
    pub estimated: Vec<usize>,
    /// The chosen filters' compressed size summed over the sections
    pub estimated_chosen: usize,
}

impl SectionFilters {
    /// The index of the strategy that won the most bytes
    #[must_use]
    pub fn dominant(&self) -> usize {
        let max = self.won.iter().max().copied().unwrap_or(0);
        self.won.iter().position(|&bytes| bytes == max).unwrap_or(0)
    }

    /// Whether the chosen filters combine several strategies. If not, they are the dominant
    /// strategy's.
    #[must_use]
    pub fn is_combined(&self) -> bool {
        self.won[self.dominant()] < self.won.iter().sum()
    }
}

impl PngImage {
    /// The first line of each section, followed by the number of lines
    #[must_use]
    pub fn section_bounds(&self) -> Vec<usize> {
        bounds(self.scan_lines(false).map(|line| line.data.len() + 1))
    }

    /// Evaluate the strategies section by section: each section gets the filters of whichever
    /// strategy compresses it smallest at the given level, following the data already chosen.
    /// Every strategy's size is summed over the sections, which compares the strategies for the
    /// whole image; an image of a single section is compressed whole.
    #[must_use]
    pub fn filter_by_sections(
        &self,
        strategies: &IndexSet<FilterStrategy>,
        optimize_alpha: bool,
        level: u8,
    ) -> SectionFilters {
        let lines: Vec<_> = self.scan_lines(false).collect();
        let bounds = bounds(lines.iter().map(|line| line.data.len() + 1));

        // The filter for each line chosen by each strategy
        let candidates: Vec<Vec<RowFilter>> = strategies
            .par_iter()
            .map(|strategy| {
                let mut filters = match strategy {
                    FilterStrategy::Basic(filter) => vec![*filter; lines.len()],
                    FilterStrategy::Predefined(filters) => filters.clone(),
                    _ => match self.choose_filters(strategy.clone(), optimize_alpha) {
                        FilterStrategy::Predefined(filters) => filters,
                        _ => unreachable!(),
                    },
                };
                // Like filter_image, lines without a predefined filter get None
                filters.resize(lines.len(), RowFilter::None);
                filters
            })
            .collect();

        let bpp = self.bytes_per_channel() * self.channels_per_pixel();
        let alpha_bytes = if optimize_alpha && self.ihdr.color_type.has_alpha() {
            self.bytes_per_channel()
        } else {
            0
        };
        let mut chosen = Vec::with_capacity(lines.len());
        let mut won = vec![0; candidates.len()];
        let mut estimated = vec![0; candidates.len()];
        let mut estimated_chosen = 0;
        // The last bytes already chosen, and the previous line as the filters see it
        // (alpha optimization may alter it)
        let mut context = Vec::new();
        let mut prev_line = Vec::new();
        for section in bounds.windows(2) {
            let (start, end) = (section[0], section[1]);
            let section_bytes: usize = lines[start..end].iter().map(|l| l.data.len() + 1).sum();
            let filters = |c: usize| &candidates[c][start..end];
            // Candidates with the same filters in this section as an earlier one share its trial
            let mut distinct = IndexSet::new();
            let mut first = Vec::new();
            let trial_of: Vec<usize> = (0..candidates.len())
                .map(|c| {
                    let (trial, new) = distinct.insert_full(filters(c));
                    if new {
                        first.push(c);
                    }
                    trial
                })
                .collect();

            // Each candidate's section after the context, its compressed size and its last line.
            // The section is filtered again rather than taken from the strategy's own output,
            // since with alpha optimization a line depends on the filters chosen before it.
            // The context is the same for every candidate, so the sizes compare the sections
            // (up to how the context itself is coded).
            let compressed_size = |data: &[u8]| {
                deflate::deflate(data, level, None)
                    .expect("the output buffer has the bound size")
                    .len()
            };
            let trial = |c: usize| {
                let mut data = Vec::with_capacity(context.len() + section_bytes);
                data.extend_from_slice(&context);
                let mut prev = prev_line.clone();
                // As in filter_image, but reusing the line buffers
                let mut line_data = Vec::new();
                for (i, &filter) in (start..end).zip(filters(c)) {
                    let line = &lines[i];
                    if i == 0 || lines[i - 1].pass != line.pass {
                        prev.clear();
                        prev.resize(line.data.len(), 0);
                    }
                    line_data.clear();
                    line_data.extend_from_slice(line.data);
                    filter.filter_line(bpp, &mut line_data, &prev, &mut data, alpha_bytes);
                    std::mem::swap(&mut prev, &mut line_data);
                }
                let size = if first.len() > 1 {
                    compressed_size(&data)
                } else {
                    0
                };
                (size, c, data, prev)
            };
            // For the estimates, take off the context compressed alone
            let (mut trials, context_size): (Vec<_>, _) = rayon::join(
                || first.par_iter().map(|&c| trial(c)).collect(),
                || {
                    if first.len() > 1 && !context.is_empty() {
                        compressed_size(&context)
                    } else {
                        0
                    }
                },
            );
            let section_size = |size: usize| size.saturating_sub(context_size);
            for (estimate, &trial) in estimated.iter_mut().zip(&trial_of) {
                *estimate += section_size(trials[trial].0);
            }
            // On equal size the earlier candidate wins
            let best = (0..trials.len())
                .min_by_key(|&t| trials[t].0)
                .expect("the first candidate is always distinct");
            let (size, c, data, prev) = trials.swap_remove(best);
            estimated_chosen += section_size(size);
            won[c] += data.len() - context.len();
            context = data[data.len().saturating_sub(WINDOW_SIZE)..].to_vec();
            prev_line = prev;
            chosen.extend_from_slice(filters(c));
        }

        SectionFilters {
            filters: chosen,
            sections: bounds.len() - 1,
            won,
            strategy_filters: candidates,
            estimated,
            estimated_chosen,
        }
    }
}

/// The first line of each section, followed by the number of lines, given each line's size.
/// Sections end at line boundaries; a short rest joins the last section.
fn bounds(line_sizes: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut bounds = vec![0];
    let mut size = 0;
    let mut lines = 0;
    for line_size in line_sizes {
        size += line_size;
        lines += 1;
        if size >= SECTION_SIZE {
            bounds.push(lines);
            size = 0;
        }
    }
    if size > 0 {
        if size < SECTION_SIZE / 2 && bounds.len() > 1 {
            bounds.pop();
        }
        bounds.push(lines);
    }
    bounds
}

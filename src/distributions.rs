use num::{cast::AsPrimitive, traits::WrappingSub, Float, PrimInt};
use statrs::distribution::{InverseCDF, Univariate};
use std::{marker::PhantomData, ops::RangeInclusive};

use super::CompressedWord;

pub trait DiscreteDistribution<S, W>
where
    W: CompressedWord,
{
    /// Returns `Ok((left_sided_cumulative, probability))` of the bin for the
    /// provided `symbol` if the symbol has nonzero probability.
    fn left_cumulative_and_probability(&self, symbol: S) -> Result<(W, W), ()>;

    /// Returns `(symbol, left_sided_cumulative, probability)` of the unique bin
    /// that satisfies `left_sided_cumulative <= quantile < right_sided_cumulative`.
    fn quantile_function(&self, quantile: W) -> (S, W, W);
}

impl<S, W, D> DiscreteDistribution<S, W> for &D
where
    W: CompressedWord,
    D: DiscreteDistribution<S, W>,
{
    fn left_cumulative_and_probability(&self, symbol: S) -> Result<(W, W), ()> {
        (*self).left_cumulative_and_probability(symbol)
    }

    fn quantile_function(&self, quantile: W) -> (S, W, W) {
        (*self).quantile_function(quantile)
    }
}

/// Builder for [`QuantizedDistribution`s]
///
/// # Example
///
/// ```
/// use statrs::distribution::Normal;
///
/// // Get a quantizer that supports integer symbols from -5 to 20, inclusively.
/// let quantizer = ans::distributions::LeakyQuantizer::new(-5..=20);
///
/// // Quantize a normal distribution with mean 8.3 and standard deviation 4.1.
/// let continuous_distribution1 = Normal::new(8.3, 4.1).unwrap();
/// let discrete_distribution1 = quantizer.quantize(continuous_distribution1);
///
/// // You can reuse the same quantizer for more than one distribution.
/// let continuous_distribution2 = Normal::new(-1.4, 2.7).unwrap();
/// let discrete_distribution2 = quantizer.quantize(continuous_distribution2);
///
/// // Use the discrete distributions with an `AnsCoder`.
/// let mut coder = ans::AnsCoder::<u32>::new();
/// coder.push_symbol(4, discrete_distribution1);
/// coder.push_symbol(-3, discrete_distribution2);
/// ```
///
/// # TODO
///
/// Implement non-leaky version once minimal const generics are stable
/// (est. February 2021).
///
/// [`QuantizedDistribution`s]: struct.QuantizedDistribution.html
pub struct LeakyQuantizer<W, S, F> {
    domain: RangeInclusive<S>,
    free_weight: F,
    phantom: PhantomData<W>,
}

impl<W, S, F> LeakyQuantizer<W, S, F>
where
    W: CompressedWord + Into<F>,
    S: PrimInt + AsPrimitive<W> + WrappingSub,
    F: Float,
{
    /// Constructs a quantizer defined on a finite domain.
    ///
    /// The bounds `&min_symbol` and `&max_symbol` are both *inclusive* and define
    /// the domain on which quantized probability mass functions will be
    /// defined. If [leaky quantization](trait.Leakiness.html) is used then all
    /// probability mass functions generated by this quantizer are guaranteed to
    /// assign a nonzero probability to all integer symbols in the range
    /// `&min_symbol..=&max_symbol`.
    pub fn new(domain: RangeInclusive<S>) -> Self {
        assert!(domain.end() >= domain.start());
        let domain_size_minus_one = domain.end().wrapping_sub(&domain.start()).as_();
        assert!(domain_size_minus_one <= W::max_value());

        let free_weight = (W::max_value() - domain_size_minus_one).into();

        Self {
            domain,
            free_weight,
            phantom: PhantomData,
        }
    }

    /// Quantizes the given continuous probability density function (PDF).
    ///
    /// Note that this method takes `self` only by reference, i.e., you can reuse
    /// the same `Quantizer` to quantize arbitrarily many PDFs.
    pub fn quantize<CD>(&self, distribution: CD) -> QuantizedDistribution<F, S, W, CD>
    where
        CD: Univariate<F, F> + InverseCDF<F>,
    {
        QuantizedDistribution {
            inner: distribution,
            quantizer: self,
        }
    }
}

/// Wrapper that turns a probability density into a [`DiscreteDistribution`]
///
/// To create a `QuantizedDistribution`, use a [`Quantizer`].
///
/// [`DiscreteDistribution`]: trait.DiscreteDistribution.html
/// [`Quantizer`]: struct.Quantizer.html
pub struct QuantizedDistribution<'a, F, S, W, CD> {
    inner: CD,
    quantizer: &'a LeakyQuantizer<W, S, F>,
}

impl<'a, F, S, W, CD> DiscreteDistribution<S, W> for QuantizedDistribution<'a, F, S, W, CD>
where
    S: PrimInt + AsPrimitive<W> + Into<F> + WrappingSub,
    F: Float + AsPrimitive<S> + AsPrimitive<W>,
    W: CompressedWord + Into<F>,
    CD: Univariate<F, F> + InverseCDF<F>,
{
    fn left_cumulative_and_probability(&self, symbol: S) -> Result<(W, W), ()> {
        let half = F::one() / (F::one() + F::one());

        let min_symbol = *self.quantizer.domain.start();
        let max_symbol = *self.quantizer.domain.end();
        let free_weight = self.quantizer.free_weight;

        if !self.quantizer.domain.contains(&symbol) {
            return Err(());
        };
        let slack = symbol.wrapping_sub(&min_symbol).as_();

        // Round both cumulatives *independently* to fixed point precision.
        let left_sided_cumulative = if symbol == min_symbol {
            // Corner case: only makes a difference if we're cutting off a fairly significant
            // left tail of the distribution.
            W::zero()
        } else {
            let non_leaky: W = (free_weight * self.inner.cdf(symbol.into() - half)).as_();
            non_leaky + slack
        };

        let right_sided_cumulative = if symbol == max_symbol {
            // Corner case: make sure that the probabilities add up to one. The generic
            // calculation in the `else` branch may lead to a lower total probability
            // because we're cutting off the right tail of the distribution and we're
            // rounding down.
            W::zero()
        } else {
            let non_leaky: W = (free_weight * self.inner.cdf(symbol.into() + half)).as_();
            non_leaky + slack + W::one()
        };

        let probability = right_sided_cumulative.wrapping_sub(&left_sided_cumulative);

        Ok((left_sided_cumulative, probability))
    }

    fn quantile_function(&self, quantile: W) -> (S, W, W) {
        let half = F::one() / (F::one() + F::one());
        let denominator = W::max_value().into() + F::one();

        let min_symbol = *self.quantizer.domain.start();
        let max_symbol = *self.quantizer.domain.end();
        let free_weight = self.quantizer.free_weight;

        // Make an initial guess for the inverse of the leaky CDF.
        let mut symbol: S = self
            .inner
            .inverse_cdf((quantile.into() + half) / denominator)
            .as_();

        let mut left_sided_cumulative = if symbol <= min_symbol {
            // Corner case: we're in the left cut off tail of the distribution.
            symbol = min_symbol;
            W::zero()
        } else {
            if symbol > max_symbol {
                // Corner case: we're in the right cut off tail of the distribution.
                symbol = max_symbol;
            }

            let non_leaky: W = (free_weight * self.inner.cdf(symbol.into() - half)).as_();
            non_leaky + symbol.wrapping_sub(&min_symbol).as_()
        };

        let right_sided_cumulative = if left_sided_cumulative > quantile {
            // Our initial guess for `symbol` was too high. Reduce it until we're good.
            symbol = symbol - S::one();
            let mut right_sided_cumulative = left_sided_cumulative;
            loop {
                if symbol == min_symbol {
                    left_sided_cumulative = W::zero();
                    break;
                }

                let non_leaky: W = (free_weight * self.inner.cdf(symbol.into() - half)).as_();
                left_sided_cumulative = non_leaky + symbol.wrapping_sub(&min_symbol).as_();
                if left_sided_cumulative <= quantile {
                    break;
                } else {
                    right_sided_cumulative = left_sided_cumulative;
                    symbol = symbol - S::one();
                }
            }

            right_sided_cumulative
        } else {
            // Our initial guess for `symbol` was either exactly right or too low.
            // Check validity of the right sided cumulative. If it isn't valid,
            // keep increasing `symbol` until it is.
            loop {
                if symbol == max_symbol {
                    break W::zero();
                }

                // FIXME: .as_() is wrong here. Wrong target + must be able to overflow
                let non_leaky: W = (free_weight * self.inner.cdf(symbol.into() + half)).as_();
                let right_sided_cumulative =
                    (non_leaky + symbol.wrapping_sub(&min_symbol).as_()).wrapping_add(&W::one());
                if right_sided_cumulative > quantile {
                    break right_sided_cumulative;
                }

                left_sided_cumulative = right_sided_cumulative;
                symbol = symbol + S::one();
            }
        };

        (
            symbol,
            left_sided_cumulative,
            right_sided_cumulative.wrapping_sub(&left_sided_cumulative),
        )
    }
}

pub struct Categorical<S, W> {
    cdf: Box<[W]>,
    min_symbol: S,
}

impl<S, W> Categorical<S, W>
where
    S: PrimInt + AsPrimitive<usize>,
    W: CompressedWord,
{
    pub fn new(probabilities: &[W], min_symbol: S) -> Self {
        let cdf = std::iter::once(&W::zero())
            .chain(probabilities)
            .scan(W::zero(), |accum, prob| {
                // This should only wrap around at last entry if `super::FREQUENCY_BITS == 32`.
                *accum = accum.wrapping_add(prob);
                Some(*accum)
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        assert!(cdf.last() == Some(&W::zero()));

        Self { min_symbol, cdf }
    }

    /// Construct a categorical distribution whose PMF approximates given probabilities.
    ///
    /// The returned distribution will be defined on the domain ranging from
    /// `min_supported_symbol` (inclusively) to
    /// `min_supported_symbol + probabilities.len()` (exclusively).
    ///
    /// The provided `probabilities` have to be nonnegative and at least one entry
    /// has to be nonzero. The provided probabilities do not necessarily need to add
    /// up to one. The resulting distribution will explicitly get normalized and an
    /// overall scaling of all entries of `probabilities` does not affect the
    /// result.
    ///
    /// The probability mass function of the returned distribution will approximate
    /// the provided probabilities as well as possible subject to the following
    /// constraints:
    /// - probabilities are represented in fixed point arithmetic, which introduces
    ///   rounding errors;
    /// - despite the possibility of rounding errors, the returned probability
    ///   distribution will be exactly normalized; and
    /// - each symbol in the domain defined above gets assigned a strictly nonzero
    ///   probability, even if the provided probability for the symbol was below the
    ///   threshold that can be resolved in fixed point arithmetic.
    ///
    /// More precisely, the resulting probability distribution minimizes the cross
    /// entropy from the provided (floating point) to the resulting (fixed point)
    /// probabilities subject to the above three constraints.
    pub fn from_continuous_probabilities<F>(probabilities: &[F], min_supported_symbol: S) -> Self
    where
        F: Float + AsPrimitive<W> + std::iter::Sum<F>,
        W: CompressedWord + Into<F> + AsPrimitive<usize>,
        usize: AsPrimitive<W>,
    {
        let probabilities = optimal_weights(probabilities);
        Self::new(&probabilities, min_supported_symbol)
    }

    /// Returns the entropy in units of bits (i.e., base 2).
    pub fn entropy<F>(&self) -> F
    where
        F: Float + std::iter::Sum,
        W: Into<F>,
    {
        let entropy_scaled = self
            .cdf
            .iter()
            .skip(1)
            .scan(W::zero(), |last, &cdf| {
                let prob = cdf.wrapping_sub(last).into();
                *last = cdf;
                Some(prob * prob.log2())
            })
            .sum::<F>();

        F::from(W::bits()).unwrap() - entropy_scaled / (W::max_value().into() + F::one())
    }
}

impl<S, W> DiscreteDistribution<S, W> for Categorical<S, W>
where
    S: PrimInt + AsPrimitive<usize>,
    W: CompressedWord,
{
    fn left_cumulative_and_probability(&self, symbol: S) -> Result<(W, W), ()> {
        assert!(symbol >= self.min_symbol);
        let index: usize = (symbol - self.min_symbol).as_();

        let (cdf, next_cdf) = unsafe {
            // SAFETY: the assertion ensures we're not out of bounds.
            assert!(index as usize + 1 < self.cdf.len());
            (
                *self.cdf.get_unchecked(index as usize),
                *self.cdf.get_unchecked(index as usize + 1),
            )
        };

        Ok((cdf, next_cdf.wrapping_sub(&cdf)))
    }

    fn quantile_function(&self, quantile: W) -> (S, W, W) {
        let mut left = 0; // Smallest possible index.
        let mut right = self.cdf.len() - 1; // One above largest possible index.

        // Binary search for the last entry of `self.cdf` that is <= quantile,
        // exploiting the fact that `self.cdf[0] == 0` and
        // `*self.cdf.last().unwrap() == 1 << super::FREQUENCY_BITS` and
        // the assumption that `quantile < 1 << super::FREQUENCY_BITS`.
        while left + 1 != right {
            let mid = (left + right) / 2;

            // SAFETY: the loop maintains the invariants
            // `0 <= left <= mid < right < self.cdf.len()` and
            // `cdf[left] <= cdf[mid] <= cdf[right]`.
            let pivot = unsafe { *self.cdf.get_unchecked(mid) };
            if pivot <= quantile {
                left = mid;
            } else {
                right = mid;
            }
        }

        // SAFETY: invariant `0 <= left < right < self.cdf.len()` still holds.
        let cdf = unsafe { *self.cdf.get_unchecked(left) };
        let next_cdf = unsafe { *self.cdf.get_unchecked(right) };

        (
            self.min_symbol + S::from(left).unwrap(),
            cdf,
            next_cdf.wrapping_sub(&cdf),
        )
    }
}

/// Finds optimal fixed-point approximation of a floating-point PMF.
///
/// The function arguments specify a (not necessarily normalized) probability
/// mass function (PMF) over `padding_left + pmf.len() + padding_right` symbols
/// that has `padding_left` zeros, followed by the probabilities in `pmf`,
/// followed by `padding_right` zeros. All entries of `pmf` must be nonnegative,
/// and `pmf` must have at least length 2.
///
/// The function returns the optimal approximation of this probability
/// distribution that is possible within fixed point arithmetic, subject to the
/// constraint that all returned probabilities are nonzero.
///
/// More precisely, the returned `Vec<W>` satisfies the following constraints:
/// - It has `padding_left + pmf.len() + padding_right` entries.
/// - All entries are nonzero (i.e., positive, since they're `W`s).
/// - The entries add up to `1 << super::FREQUENCY_BITS` (ignoring wrapping).
/// - Up to the above three constraints and an overall scaling by
///   `1 << super::FREQUENCY_BITS`, the returned vector optimally approximates
///   the provided probability distribution by minimizing the cross entropy:
///   `cross_entropy = - sum_i[padded_pmf[i] * log(returned[i])]`,
///   where `padded_pmf` is the provided `pmf` padded with `padding_left` and
///   `padding_right` zeros to the left and right, respectively, and `returned`
///   is the return value.
///
/// # TODO
///
/// Implement non-leaky variant of this
fn optimal_weights<F, W>(pmf: &[F]) -> Vec<W>
where
    F: Float + AsPrimitive<W> + std::iter::Sum<F>,
    W: CompressedWord + Into<F> + AsPrimitive<usize>,
    usize: AsPrimitive<W>,
{
    assert!(!pmf.is_empty());
    assert!(pmf.len() <= W::max_value().as_());
    if pmf.len() == 1 {
        // The algorithm below doesn't work on degenerate distributions, so we treat
        // them as a special case.
        return vec![W::zero()];
    }

    // Start by assigning each symbol weight 1 and then distributing no more than
    // the remaining weight approximately evenly across all symbols.
    let free_weight = W::zero().wrapping_sub(&pmf.len().as_());
    let mut remaining_free_weight = free_weight;
    let free_weight_float: F = free_weight.into();
    let scale = free_weight_float / pmf.iter().cloned().sum::<F>();

    let mut indices_probs_weights_wins_losses = pmf
        .iter()
        .enumerate()
        .map(|(index, &prob)| {
            let current_free_weight = (prob * scale).as_();
            remaining_free_weight = remaining_free_weight - current_free_weight;
            let weight = current_free_weight + W::one();

            // How much the cross entropy would decrease when increasing the weight by one.
            let win = prob * (F::one() / weight.into()).ln_1p();

            // How much the cross entropy would increase when decreasing the weight by one.
            let loss = if weight == W::one() {
                F::infinity()
            } else {
                -prob * (-F::one() / weight.into()).ln_1p()
            };

            (index, prob, weight, win, loss)
        })
        .collect::<Vec<_>>();

    // Distribute remaining weight evenly among symbols with highest wins.
    while remaining_free_weight != W::zero() {
        indices_probs_weights_wins_losses
            .sort_by(|&(_, _, _, win1, _), &(_, _, _, win2, _)| win2.partial_cmp(&win1).unwrap());
        let batch_size = std::cmp::min(
            remaining_free_weight.as_(),
            indices_probs_weights_wins_losses.len(),
        );
        for (_, prob, weight, win, loss) in &mut indices_probs_weights_wins_losses[..batch_size] {
            *weight = *weight + W::one(); // Cannot end up in `max_weight` because win would otherwise be -infinity.
            *win = *prob * (F::one() / (*weight).into()).ln_1p();
            *loss = -*prob * (-F::one() / (*weight).into()).ln_1p();
        }
        remaining_free_weight = remaining_free_weight - batch_size.as_();
    }

    loop {
        // Find element where increasing weight would incur the biggest win.
        let (buyer_index, &(_, _, _, buyer_win, _)) = indices_probs_weights_wins_losses
            .iter()
            .enumerate()
            .max_by(|(_, (_, _, _, win1, _)), (_, (_, _, _, win2, _))| {
                win1.partial_cmp(win2).unwrap()
            })
            .unwrap();
        let (seller_index, (_, seller_prob, seller_weight, seller_win, seller_loss)) =
            indices_probs_weights_wins_losses
                .iter_mut()
                .enumerate()
                .min_by(|(_, (_, _, _, _, loss1)), (_, (_, _, _, _, loss2))| {
                    loss1.partial_cmp(loss2).unwrap()
                })
                .unwrap();

        if buyer_index == seller_index {
            // This can only happen due to rounding errors. In this case, we can't expect
            // to be able to improve further.
            break;
        }

        if buyer_win <= *seller_loss {
            // We've found the optimal solution.
            break;
        }

        *seller_weight = *seller_weight - W::one();
        *seller_win = *seller_prob * (F::one() / (*seller_weight).into()).ln_1p();
        *seller_loss = if *seller_weight == W::one() {
            F::infinity()
        } else {
            -*seller_prob * (-F::one() / (*seller_weight).into()).ln_1p()
        };

        let (_, buyer_prob, buyer_weight, buyer_win, buyer_loss) =
            &mut indices_probs_weights_wins_losses[buyer_index];
        *buyer_weight = *buyer_weight + W::one();
        *buyer_win = *buyer_prob * (F::one() / (*buyer_weight).into()).ln_1p();
        *buyer_loss = -*buyer_prob * (-F::one() / (*buyer_weight).into()).ln_1p();
    }

    indices_probs_weights_wins_losses.sort_by_key(|&(index, _, _, _, _)| index);

    indices_probs_weights_wins_losses
        .into_iter()
        .map(|(_, _, weight, _, _)| weight)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use statrs::distribution::Normal;

    #[test]
    fn leaky_quantized_normal() {
        let quantizer = LeakyQuantizer::new(-127..=127);

        for &std_dev in &[0.0001, 0.1, 3.5, 123.45, 1234.56] {
            for &mean in &[-300.6, -100.2, -5.2, 0.0, 50.3, 180.2, 2000.0] {
                let continuous_distribution = Normal::new(mean, std_dev).unwrap();
                test_discrete_distribution(quantizer.quantize(continuous_distribution), -127..128);
            }
        }
    }

    /// Test that `optimal_weights` reproduces the same distribution when fed with an
    /// already quantized distribution.
    #[test]
    fn trivial_optimal_weights() {
        let hist = [
            56319u32, 134860032, 47755520, 60775168, 75699200, 92529920, 111023616, 130420736,
            150257408, 169970176, 188869632, 424260864, 229548800, 236082432, 238252287, 234666240,
            1, 1, 227725568, 216746240, 202127104, 185095936, 166533632, 146508800, 126643712,
            107187968, 88985600, 72576000, 57896448, 45617664, 34893056, 26408448, 19666688,
            14218240, 10050048, 7164928, 13892864,
        ];
        assert_eq!(hist.iter().map(|&x| x as u64).sum::<u64>(), 1 << 32);

        let probabilities = hist.iter().map(|&x| x as f64).collect::<Vec<_>>();
        let weights: Vec<u32> = optimal_weights(&probabilities);

        assert_eq!(&weights[..], &hist[..]);
    }

    #[test]
    fn nontrivial_optimal_weights() {
        let hist = [
            1u32, 186545, 237403, 295700, 361445, 433686, 509456, 586943, 663946, 737772, 1657269,
            896675, 922197, 930672, 916665, 0, 0, 0, 0, 0, 723031, 650522, 572300, 494702, 418703,
            347600, 1, 283500, 226158, 178194, 136301, 103158, 76823, 55540, 39258, 27988, 54269,
        ];
        assert_ne!(hist.iter().map(|&x| x as u64).sum::<u64>(), 1 << 32);

        let probabilities = hist.iter().map(|&x| x as f64).collect::<Vec<_>>();
        let weights: Vec<u32> = optimal_weights(&probabilities);

        assert_eq!(weights.len(), hist.len());
        assert_eq!(weights.iter().map(|&x| x as u64).sum::<u64>(), 1 << 32);
        for &w in &weights {
            assert!(w > 0);
        }

        let mut weights_and_hist = weights
            .iter()
            .cloned()
            .zip(hist.iter().cloned())
            .collect::<Vec<_>>();

        // Check that sorting by weight is compatible with sorting by hist.
        weights_and_hist.sort();
        // TODO: replace the following with
        // `assert!(weights_and_hist.iter().map(|&(_, x)| x).is_sorted())`
        // when `is_sorted` becomes stable.
        let mut last = 0;
        for (_, hist) in weights_and_hist {
            assert!(hist >= last);
            last = hist;
        }
    }

    #[test]
    fn categorical() {
        let hist = [
            1u32, 186545, 237403, 295700, 361445, 433686, 509456, 586943, 663946, 737772, 1657269,
            896675, 922197, 930672, 916665, 0, 0, 0, 0, 0, 723031, 650522, 572300, 494702, 418703,
            347600, 1, 283500, 226158, 178194, 136301, 103158, 76823, 55540, 39258, 27988, 54269,
        ];
        let probabilities = hist.iter().map(|&x| x as f64).collect::<Vec<_>>();

        let distribution = Categorical::from_continuous_probabilities(&probabilities, -127);
        test_discrete_distribution(distribution, -127..-90);
    }

    fn test_discrete_distribution(
        distribution: impl DiscreteDistribution<i32, u32>,
        domain: std::ops::Range<i32>,
    ) {
        let mut sum = 0;

        for symbol in domain {
            let (left_cumulative, prob) = distribution
                .left_cumulative_and_probability(symbol)
                .unwrap();
            assert_eq!(left_cumulative as u64, sum);
            sum += prob as u64;

            let expected = (symbol, left_cumulative, prob);
            assert_eq!(distribution.quantile_function(left_cumulative), expected);
            assert_eq!(distribution.quantile_function((sum - 1) as u32), expected);
            assert_eq!(
                distribution.quantile_function(left_cumulative + prob / 2),
                expected
            );
        }

        assert_eq!(sum, 1 << 32);
    }
}

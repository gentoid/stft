/*!

Computes the [short-time fourier transform](https://en.wikipedia.org/wiki/Short-time_Fourier_transform)
on streaming data.

## example

```
use stft::{STFT, WindowType};

// Generate ten seconds of fake audio
let sample_rate: usize = 44100;
let seconds: usize = 10;
let sample_count = sample_rate * seconds;
let all_samples = (0..sample_count).map(|x| x as f64).collect::<Vec<f64>>();

// Initialize the short-time fourier transform
let window_type: WindowType = WindowType::Hanning;
let window_size: usize = 1024;
let step_size: usize = 512;
let mut stft = STFT::new(window_type, window_size, step_size);

// We need a buffer to hold a computed column of the spectrogram
let mut spectrogram_column: Vec<f64> =
    std::iter::repeat(0.).take(stft.output_size()).collect();

// Iterate over all the samples in chunks of 3000 samples.
// In a real program you would probably read from a stream instead.
for some_samples in (&all_samples[..]).chunks(3000) {
    // Append the samples to the internal ringbuffer of the stft
    stft.append_samples(some_samples);

    // Loop as long as there remain window_size samples in the internal
    // ringbuffer of the stft
    while stft.contains_enough_to_compute() {
        // Compute one column of the stft by
        // taking the first window_size samples of the internal ringbuffer,
        // multiplying them with the window,
        // computing the fast fourier transform,
        // taking half of the symetric complex outputs,
        // computing the norm of the complex outputs and
        // taking the log10
        stft.compute_column(&mut spectrogram_column[..]);

        // Here's where you would do something with the
        // spectrogram_column...

        // Drop step_size samples from the internal ringbuffer of the stft
        // making a step of size step_size
        stft.move_to_next_column();
    }
}

assert!(!stft.is_empty())

```
*/

use std::str::FromStr;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::num_traits::{Float, Signed, Zero};
use rustfft::{FFTnum, FFTplanner, FFT};

use strider::{SliceRing, SliceRingImpl};

pub struct STFT<T>
where
    T: FFTnum + FromF64 + Float,
{
    pub window_size: usize,
    pub step_size: usize,
    pub fft: Arc<dyn FFT<T>>,
    pub window: Option<Vec<T>>,
    pub sample_ring: SliceRingImpl<T>,
    pub real_input: Vec<T>,
    pub complex_input: Vec<Complex<T>>,
    pub complex_output: Vec<Complex<T>>,
}

impl<T> STFT<T>
where
    T: FFTnum + FromF64 + Float,
{
    pub fn new(window_type: WindowType, window_size: usize, step_size: usize) -> Self {
        let window = Self::window_type_to_window_vec(window_type, window_size);
        Self::new_with_window_vec(window, window_size, step_size)
    }

    pub fn output_size(&self) -> usize {
        self.window_size / 2
    }

    pub fn len(&self) -> usize {
        self.sample_ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn append_samples(&mut self, input: &[T]) {
        self.sample_ring.push_many_back(input);
    }

    pub fn contains_enough_to_compute(&self) -> bool {
        self.window_size <= self.sample_ring.len()
    }

    // TODO: use `Result`s instead of panics
    /// Computes a column of the spectrogram
    /// # Panics
    /// panics unless `self.output_size() == output.len()`
    pub fn compute_column(&mut self, output: &mut [T]) {
        assert_eq!(self.output_size(), output.len());

        self.compute_into_complex_output();

        for (dst, src) in output.iter_mut().zip(self.complex_output.iter()) {
            *dst = log10_positive(src.norm());
        }
    }

    /// # Panics
    /// panics unless `self.output_size() == output.len()`
    pub fn compute_complex_column(&mut self, output: &mut [Complex<T>]) {
        assert_eq!(self.output_size(), output.len());

        self.compute_into_complex_output();

        for (dst, src) in output.iter_mut().zip(self.complex_output.iter()) {
            *dst = *src;
        }
    }

    /// # Panics
    /// panics unless `self.output_size() == output.len()`
    pub fn compute_magnitude_column(&mut self, output: &mut [T]) {
        assert_eq!(self.output_size(), output.len());

        self.compute_into_complex_output();

        for (dst, src) in output.iter_mut().zip(self.complex_output.iter()) {
            *dst = src.norm();
        }
    }

    /// Make a step
    /// Drops `self.step_size` samples from the internal buffer `self.sample_ring`.
    pub fn move_to_next_column(&mut self) {
        self.sample_ring.drop_many_front(self.step_size);
    }

    // TODO this should ideally take an iterator and not a vec
    fn new_with_window_vec(window: Option<Vec<T>>, window_size: usize, step_size: usize) -> Self {
        // TODO more assertions:
        // window_size is power of two
        // step_size > 0
        assert!(step_size <= window_size);
        let inverse = false;
        let mut planner = FFTplanner::new(inverse);
        STFT {
            window_size,
            step_size,
            fft: planner.plan_fft(window_size),
            sample_ring: SliceRingImpl::new(),
            window,
            real_input: std::iter::repeat(T::zero()).take(window_size).collect(),
            complex_input: std::iter::repeat(Complex::<T>::zero())
                .take(window_size)
                .collect(),
            complex_output: std::iter::repeat(Complex::<T>::zero())
                .take(window_size)
                .collect(),
        }
    }

    fn window_type_to_window_vec(window_type: WindowType, window_size: usize) -> Option<Vec<T>> {
        match window_type {
            WindowType::Hanning => Some(
                apodize::hanning_iter(window_size)
                    .map(FromF64::from_f64)
                    .collect(),
            ),
            WindowType::Hamming => Some(
                apodize::hamming_iter(window_size)
                    .map(FromF64::from_f64)
                    .collect(),
            ),
            WindowType::Blackman => Some(
                apodize::blackman_iter(window_size)
                    .map(FromF64::from_f64)
                    .collect(),
            ),
            WindowType::Nuttall => Some(
                apodize::nuttall_iter(window_size)
                    .map(FromF64::from_f64)
                    .collect(),
            ),
            WindowType::None => None,
        }
    }

    fn compute_into_complex_output(&mut self) {
        assert!(self.contains_enough_to_compute());

        // Read into real_input
        self.sample_ring.read_many_front(&mut self.real_input[..]);

        // Multiply real_input with window
        if let Some(ref window) = self.window {
            for (dst, src) in self.real_input.iter_mut().zip(window.iter()) {
                *dst = *dst * *src;
            }
        }

        // Copy windowed real_input as real parts into complex_input
        for (dst, src) in self.complex_input.iter_mut().zip(self.real_input.iter()) {
            dst.re = *src;
        }

        // Compute fft
        self.fft
            .process(&mut self.complex_input, &mut self.complex_output);
    }
}

pub trait FromF64 {
    fn from_f64(n: f64) -> Self;
}

impl FromF64 for f64 {
    fn from_f64(n: f64) -> Self {
        n
    }
}

impl FromF64 for f32 {
    fn from_f64(n: f64) -> Self {
        n as f32
    }
}

/// The type of apodization window to use
#[derive(Clone, Copy, PartialEq, PartialOrd, Eq, Ord, Debug, Hash)]
pub enum WindowType {
    Hanning,
    Hamming,
    Blackman,
    Nuttall,
    None,
}

impl FromStr for WindowType {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_lowercase();
        match &lower[..] {
            "hanning" => Ok(WindowType::Hanning),
            "hann" => Ok(WindowType::Hanning),
            "hamming" => Ok(WindowType::Hamming),
            "blackman" => Ok(WindowType::Blackman),
            "nuttall" => Ok(WindowType::Nuttall),
            "none" => Ok(WindowType::None),
            _ => Err("no match"),
        }
    }
}

impl std::fmt::Display for WindowType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "{:?}", self)
    }
}

/// Returns `0` if `log10(value).is_negative()`,
/// otherwise returns `log10(value)`.
/// `log10` turns values in domain `0..1` into values
/// in range `-inf..0`.
/// `log10_positive` turns values in domain `0..1` into `0`.
/// This sets very small values to zero which may not be
/// what you want depending on your application.
fn log10_positive<T: Float + Signed + Zero>(value: T) -> T {
    let log = value.log10();
    if log.is_negative() {
        T::zero()
    } else {
        log
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_type_from_string() {
        assert_eq!(
            WindowType::from_str("Hanning").unwrap(),
            WindowType::Hanning
        );
        assert_eq!(
            WindowType::from_str("hanning").unwrap(),
            WindowType::Hanning
        );
        assert_eq!(WindowType::from_str("hann").unwrap(), WindowType::Hanning);
        assert_eq!(
            WindowType::from_str("blackman").unwrap(),
            WindowType::Blackman
        );
    }

    #[test]
    fn test_window_type_to_string() {
        assert_eq!(WindowType::Hanning.to_string(), "Hanning");
    }

    #[test]
    fn test_log10_positive() {
        assert_eq!(log10_positive(-1.), 0.);
        assert_eq!(log10_positive(0.), 0.);
        assert_eq!(log10_positive(1.), 0.);
        assert_eq!(log10_positive(10.), 1.);
        assert_eq!(log10_positive(100.), 2.);
        assert_eq!(log10_positive(1000.), 3.);
    }

    #[test]
    fn test_stft() {
        let mut stft = STFT::new(WindowType::Hanning, 8, 4);
        assert!(!stft.contains_enough_to_compute());
        assert_eq!(stft.output_size(), 4);
        assert_eq!(stft.len(), 0);
        stft.append_samples(&vec![500., 0., 100.][..]);
        assert_eq!(stft.len(), 3);
        assert!(!stft.contains_enough_to_compute());
        stft.append_samples(&vec![500., 0., 100., 0.][..]);
        assert_eq!(stft.len(), 7);
        assert!(!stft.contains_enough_to_compute());

        stft.append_samples(&vec![500.][..]);
        assert!(stft.contains_enough_to_compute());

        let mut output: Vec<f64> = vec![0.; 4];
        stft.compute_column(&mut output[..]);
        println!("{:?}", output);
    }
}

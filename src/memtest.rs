use {
    crate::prelude::*,
    rand::random,
    serde::{Deserialize, Deserializer, Serialize, Serializer},
    std::{error::Error, fmt},
};

// TODO: Intend to convert this module to a standalone `no_std` crate

pub type MemtestResult<O> = Result<MemtestOutcome, MemtestError<<O as TestObserver>::Error>>;

#[derive(Debug, Serialize, Deserialize)]
#[must_use]
pub enum MemtestOutcome {
    Pass,
    Fail(MemtestFailure),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MemtestFailure {
    /// Failure due to the actual value read being different from the expected value
    UnexpectedValue {
        address: usize,
        expected: usize,
        actual: usize,
    },
    /// Failure due to the two memory locations being compared returning two different values
    /// This is used by tests where memory is split in two and values are written in pairs
    MismatchedValues {
        address1: usize,
        value1: usize,
        address2: usize,
        value2: usize,
    },
}

pub trait TestObserver {
    type Error: Error;
    fn init(&mut self, expected_iter: u64);
    fn check(&mut self) -> Result<(), Self::Error>;
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MemtestError<E: Error> {
    Observer(E),
    #[serde(
        serialize_with = "serialize_memtest_error_other",
        deserialize_with = "deserialize_memtest_error_other"
    )]
    Other(anyhow::Error),
}

macro_rules! memtest_kinds {{
    $($variant: ident),* $(,)?
} => {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum MemtestKind {
        $($variant,)*
    }

    impl MemtestKind {
        pub const ALL: &[Self] = &[
            $(Self::$variant),*
        ];
    }

    impl std::str::FromStr for MemtestKind {
        type Err = ParseMemtestKindError;
        fn from_str(s: &str) -> Result<Self, Self::Err> {
            match s {
            $(
                stringify!($variant) => Ok(Self::$variant),
            )*
                _ => Err(ParseMemtestKindError),
            }
        }
    }
}}

memtest_kinds! {
    OwnAddressBasic,
    OwnAddressRepeat,
    RandomVal,
    Xor,
    Sub,
    Mul,
    Div,
    Or,
    And,
    SeqInc,
    SolidBits,
    Checkerboard,
    BlockSeq,
    MovInvFixedBlock,
    MovInvFixedBit,
    MovInvFixedRandom,
    MovInvWalk,
    MovInvRandom,
    Modulo20,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseMemtestKindError;

/// Write the address of each memory location to itself, then read back the value and check that it
/// matches the expected address.
#[tracing::instrument(skip_all)]
pub fn test_own_address_basic<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul(2))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        write_volatile_safe(mem_ref, address_from_ref(mem_ref));
    }

    for mem_ref in memory.iter() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        let address = address_from_ref(mem_ref);
        let actual = read_volatile_safe(mem_ref);

        if actual != address {
            info!("Test failed at 0x{address:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                address,
                expected: address,
                actual,
            }));
        }
    }

    Ok(MemtestOutcome::Pass)
}

/// Write the address of each memory location (or its complement) to itself, then read back the
/// value and check that it matches the expected address.
/// This procedure is repeated 16 times.
#[tracing::instrument(skip_all)]
pub fn test_own_address_repeat<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    const NUM_RUNS: u64 = 16;
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul(2 * NUM_RUNS))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    let val_to_write = |address: usize, i, j| {
        if (i + j) % 2 == 0 {
            address
        } else {
            !(address)
        }
    };

    for i in 0..usize::try_from(NUM_RUNS).unwrap() {
        for (j, mem_ref) in memory.iter_mut().enumerate() {
            observer.check().map_err(|e| MemtestError::Observer(e))?;
            let val = val_to_write(address_from_ref(mem_ref), i, j);
            write_volatile_safe(mem_ref, val);
        }

        for (j, mem_ref) in memory.iter().enumerate() {
            observer.check().map_err(|e| MemtestError::Observer(e))?;
            let address = address_from_ref(mem_ref);
            let expected = val_to_write(address, i, j);
            let actual = read_volatile_safe(mem_ref);

            if actual != expected {
                info!("Test failed at 0x{address:x}");
                return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                    address,
                    expected,
                    actual,
                }));
            }
        }
    }

    Ok(MemtestOutcome::Pass)
}

/// Split given memory into two halves and iterate through memory locations in pairs. For each
/// pair, write a random value. After all locations are written, read and compare the two halves.
#[tracing::instrument(skip_all)]
pub fn test_random_val<O: TestObserver>(memory: &mut [usize], mut observer: O) -> MemtestResult<O> {
    let (first_half, second_half) = split_slice_in_half(memory)?;
    let expected_iter =
        u64::try_from(first_half.len() * 2).context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    for (first_ref, second_ref) in first_half.iter_mut().zip(second_half.iter_mut()) {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        let val = random();
        write_volatile_safe(first_ref, val);
        write_volatile_safe(second_ref, val);
    }

    compare_regions(first_half, second_half, &mut observer)
}

/// Reset all bits in given memory to 1s. Split given memory into two halves and iterate through
/// memory locations in pairs. For each pair, write the XOR result of a random value and the value
/// read from the location. After all locations are written, read and compare the two halves.
#[tracing::instrument(skip_all)]
pub fn test_xor<O: TestObserver>(memory: &mut [usize], observer: O) -> MemtestResult<O> {
    test_two_regions(memory, observer, std::ops::BitXor::bitxor)
}

/// Reset all bits in given memory to 1s. Split given memory into two halves and iterate through
/// memory locations in pairs. For each pair, write the result of subtracting a random value from
/// the value read from the location. After all locations are written, read and compare the two
/// halves.
#[tracing::instrument(skip_all)]
pub fn test_sub<O: TestObserver>(memory: &mut [usize], observer: O) -> MemtestResult<O> {
    test_two_regions(memory, observer, usize::wrapping_sub)
}

/// Reset all bits in given memory to 1s. Split given memory into two halves and iterate through
/// memory locations in pairs. For each pair, write the result of multiplying a random value with
/// the value read from the location. After all locations are written, read and compare the two
/// halves.
#[tracing::instrument(skip_all)]
pub fn test_mul<O: TestObserver>(memory: &mut [usize], observer: O) -> MemtestResult<O> {
    test_two_regions(memory, observer, usize::wrapping_mul)
}

/// Reset all bits in given memory to 1s. Split given memory into two halves and iterate through
/// memory locations in pairs. For each pair, write the result of dividing the value read from the
/// location with a random value. After all locations are written, read and compare the two halves.
#[tracing::instrument(skip_all)]
pub fn test_div<O: TestObserver>(memory: &mut [usize], observer: O) -> MemtestResult<O> {
    test_two_regions(memory, observer, |n, d| n.wrapping_div(usize::max(d, 1)))
}

/// Reset all bits in given memory to 1s. Split given memory into two halves and iterate through
/// memory locations in pairs. For each pair, write the OR result of a random value and the value
/// read from the location. After all locations are written, read and compare the two halves.
#[tracing::instrument(skip_all)]
pub fn test_or<O: TestObserver>(memory: &mut [usize], observer: O) -> MemtestResult<O> {
    test_two_regions(memory, observer, std::ops::BitOr::bitor)
}

/// Reset all bits in given memory to 1s. Split given memory into two halves and iterate through
/// memory locations in pairs. For each pair, write the AND result of a random value and the value
/// read from the location. After all locations are written, read and compare the two halves.
#[tracing::instrument(skip_all)]
pub fn test_and<O: TestObserver>(memory: &mut [usize], observer: O) -> MemtestResult<O> {
    test_two_regions(memory, observer, std::ops::BitAnd::bitand)
}

/// Base function for `test_xor`, `test_sub`, `test_mul`, `test_div`, `test_or` and `test_and`
///
/// Reset all bits in given memory to 1s. Split given memory into two halves and iterate through
/// memory locations in pairs. Write to each pair using the given `write_val` function. After all
/// locations are written, read and compare the two halves.
fn test_two_regions<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
    transform_fn: fn(usize, usize) -> usize,
) -> MemtestResult<O> {
    mem_reset(memory);
    let (first_half, second_half) = split_slice_in_half(memory)?;
    let expected_iter =
        u64::try_from(first_half.len() * 2).context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    for (first_ref, second_ref) in first_half.iter_mut().zip(second_half.iter_mut()) {
        observer.check().map_err(|e| MemtestError::Observer(e))?;

        let mixing_val = random();

        let val = read_volatile_safe(first_ref);
        let new_val = transform_fn(val, mixing_val);
        write_volatile_safe(first_ref, new_val);

        let val = read_volatile_safe(second_ref);
        let new_val = transform_fn(val, mixing_val);
        write_volatile_safe(second_ref, new_val);
    }

    compare_regions(first_half, second_half, &mut observer)
}

/// Split given memory into two halves and iterate through memory locations in pairs. Generate a
/// random value at the start. For each pair, write the result of adding the random value and the
/// index of iteration. After all locations are written, read and compare the two halves.
#[tracing::instrument(skip_all)]
pub fn test_seq_inc<O: TestObserver>(memory: &mut [usize], mut observer: O) -> MemtestResult<O> {
    let (first_half, second_half) = split_slice_in_half(memory)?;
    let expected_iter =
        u64::try_from(first_half.len() * 2).context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    let mut val: usize = random();
    for (first_ref, second_ref) in first_half.iter_mut().zip(second_half.iter_mut()) {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        val = val.wrapping_add(1);
        write_volatile_safe(first_ref, val);
        write_volatile_safe(second_ref, val);
    }

    compare_regions(first_half, second_half, &mut observer)
}

/// Split given memory into two halves and iterate through memory locations in pairs. For each
/// pair, write to all bits as either 1s or 0s, alternating after each memory location pair.
/// After all locations are written, read and compare the two halves.
/// This procedure is repeated 64 times.
#[tracing::instrument(skip_all)]
pub fn test_solid_bits<O: TestObserver>(memory: &mut [usize], mut observer: O) -> MemtestResult<O> {
    const NUM_RUNS: u64 = 64;
    let (first_half, second_half) = split_slice_in_half(memory)?;
    let expected_iter = u64::try_from(first_half.len() * 2)
        .ok()
        .and_then(|count| count.checked_mul(NUM_RUNS))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    let mut solid_bits = !0;
    for _ in 0..NUM_RUNS {
        solid_bits = !solid_bits;
        let mut val = solid_bits;

        for (first_ref, second_ref) in first_half.iter_mut().zip(second_half.iter_mut()) {
            observer.check().map_err(|e| MemtestError::Observer(e))?;
            val = !val;
            write_volatile_safe(first_ref, val);
            write_volatile_safe(second_ref, val);
        }

        if let MemtestOutcome::Fail(f) = compare_regions(first_half, second_half, &mut observer)? {
            return Ok(MemtestOutcome::Fail(f));
        }
    }
    Ok(MemtestOutcome::Pass)
}

/// Split given memory into two halves and iterate through memory locations in pairs. For each pair,
/// write to a pattern of alternating 1s and 0s (in bytes it is either 0x55 or 0xaa, and alternating
/// after each memory location pair). After all locations are written, read and compare the two
/// halves.
/// This procedure is repeated 64 times.
#[tracing::instrument(skip_all)]
pub fn test_checkerboard<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    const NUM_RUNS: u64 = 64;
    let (first_half, second_half) = split_slice_in_half(memory)?;
    let expected_iter = u64::try_from(first_half.len() * 2)
        .ok()
        .and_then(|count| count.checked_mul(NUM_RUNS))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    let mut checker_board = usize_filled_from_byte(0xaa);

    for _ in 0..NUM_RUNS {
        checker_board = !checker_board;
        let mut val = checker_board;

        for (first_ref, second_ref) in first_half.iter_mut().zip(second_half.iter_mut()) {
            observer.check().map_err(|e| MemtestError::Observer(e))?;
            val = !val;
            write_volatile_safe(first_ref, val);
            write_volatile_safe(second_ref, val);
        }

        if let MemtestOutcome::Fail(f) = compare_regions(first_half, second_half, &mut observer)? {
            return Ok(MemtestOutcome::Fail(f));
        }
    }
    Ok(MemtestOutcome::Pass)
}

/// Split given memory into two halves and iterate through memory locations in pairs. For each pair,
/// write to all bytes with the value i. After all locations are written, read and compare the two
/// halves.
/// This procedure is repeated 256 times, with i corresponding to the iteration number 0-255.
#[tracing::instrument(skip_all)]
pub fn test_block_seq<O: TestObserver>(memory: &mut [usize], mut observer: O) -> MemtestResult<O> {
    const NUM_RUNS: u64 = 256;
    let (first_half, second_half) = split_slice_in_half(memory)?;
    let expected_iter = u64::try_from(first_half.len() * 2)
        .ok()
        .and_then(|count| count.checked_mul(NUM_RUNS))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    for i in 0..=(u8::try_from(NUM_RUNS - 1).unwrap()) {
        let val = usize_filled_from_byte(i);

        for (first_ref, second_ref) in first_half.iter_mut().zip(second_half.iter_mut()) {
            observer.check().map_err(|e| MemtestError::Observer(e))?;
            write_volatile_safe(first_ref, val);
            write_volatile_safe(second_ref, val);
        }

        if let MemtestOutcome::Fail(f) = compare_regions(first_half, second_half, &mut observer)? {
            return Ok(MemtestOutcome::Fail(f));
        }
    }
    Ok(MemtestOutcome::Pass)
}

// Each call to the moving inversion algorithm iterates through the memory 3 times
const MOV_INV_ITERATIONS: u64 = 3;

/// This test adapts the moving inversion algorithm implemented by [memtest86+](https://github.com/
/// memtest86plus/memtest86plus). As described in the the [Memtest86+ Test Algorithm Section](https://github.com/
/// memtest86plus/memtest86plus?tab=readme-ov-file#memtest86-test-algorithms),
///
/// "The moving inversion tests work as follows:
/// 1. Fill memory with a pattern
/// 2. Starting at the lowest address
///     i. check that the pattern has not changed
///     ii. write the pattern's complement
///     iii. increment the address
///     iv. repeat 2.1 to 2.3
/// 3. Starting at the highest address
///     i. check that the pattern has not changed
///     ii. write the pattern's complement
///     iii. decrement the address
///     iv. repeat 3.1 to 3.3 "
///
/// This test runs the moving inversion algorithm with fixed patterns of all bits as 1s or 0s.
#[tracing::instrument(skip_all)]
pub fn test_mov_inv_fixed_block<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul(MOV_INV_ITERATIONS * 2))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    if let MemtestOutcome::Fail(f) = mov_inv_fixed_pattern(memory, 0, &mut observer)? {
        return Ok(MemtestOutcome::Fail(f));
    }
    mov_inv_fixed_pattern(memory, !0, &mut observer)
}

/// This test adapts the moving inversion algorithm implemented by [memtest86+](https://github.com/
/// memtest86plus/memtest86plus). For a detailed explanation of the algorithm, please refer
/// to the description available at the [Memtest86+ Test Algorithm Section](https://github.com/
/// memtest86plus/memtest86plus?tab=readme-ov-file#memtest86-test-algorithms)
///
/// This test runs the moving inversion algorithm with fixed 8-bit patterns where 1 bit is 1/0 and the
/// other 7 bits are 0/1s.  The procedure is repeated 8 times with the pattern rotated by 1 bit each
/// time to test all bits in a byte.
#[tracing::instrument(skip_all)]
pub fn test_mov_inv_fixed_bit<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    const NUM_RUNS: u64 = 8;
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul(MOV_INV_ITERATIONS * 2 * NUM_RUNS))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    let mut pattern = usize_filled_from_byte(0x10);
    for _ in 0..=(u8::try_from(NUM_RUNS - 1).unwrap()) {
        if let MemtestOutcome::Fail(f) = mov_inv_fixed_pattern(memory, pattern, &mut observer)? {
            return Ok(MemtestOutcome::Fail(f));
        }
        if let MemtestOutcome::Fail(f) = mov_inv_fixed_pattern(memory, !pattern, &mut observer)? {
            return Ok(MemtestOutcome::Fail(f));
        }
        pattern = pattern.rotate_right(1);
    }
    Ok(MemtestOutcome::Pass)
}

/// This test adapts the moving inversion algorithm implemented by [memtest86+](https://github.com/
/// memtest86plus/memtest86plus). For a detailed explanation of the algorithm, please refer
/// to the description available at the [Memtest86+ Test Algorithm Section](https://github.com/
/// memtest86plus/memtest86plus?tab=readme-ov-file#memtest86-test-algorithms)
///
/// This test runs the moving inversion algorithm with random fixed patterns.
#[tracing::instrument(skip_all)]
pub fn test_mov_inv_fixed_random<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul(MOV_INV_ITERATIONS * 2))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    let pattern = random();
    if let MemtestOutcome::Fail(f) = mov_inv_fixed_pattern(memory, pattern, &mut observer)? {
        return Ok(MemtestOutcome::Fail(f));
    }
    mov_inv_fixed_pattern(memory, !pattern, &mut observer)
}

fn mov_inv_fixed_pattern<O: TestObserver>(
    memory: &mut [usize],
    pattern: usize,
    observer: &mut O,
) -> MemtestResult<O> {
    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        write_volatile_safe(mem_ref, pattern);
    }

    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        let address = address_from_ref(mem_ref);
        let actual = read_volatile_safe(mem_ref);

        if actual != pattern {
            info!("Test failed at 0x{address:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                address,
                expected: pattern,
                actual,
            }));
        }

        write_volatile_safe(mem_ref, !pattern);
    }

    for mem_ref in memory.iter_mut().rev() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        let address = address_from_ref(mem_ref);
        let actual = read_volatile_safe(mem_ref);

        if actual != !pattern {
            info!("Test failed at 0x{address:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                address,
                expected: !pattern,
                actual,
            }));
        }

        write_volatile_safe(mem_ref, pattern);
    }

    Ok(MemtestOutcome::Pass)
}

/// This test adapts the moving inversion algorithm implemented by [memtest86+](https://github.com/
/// memtest86plus/memtest86plus). For a detailed explanation of the algorithm, please refer
/// to the description available at the [Memtest86+ Test Algorithm Section](https://github.com/
/// memtest86plus/memtest86plus?tab=readme-ov-file#memtest86-test-algorithms)
///
/// This test runs the moving inversion algorithm with a "walking" bit pattern. The algorithm starts
/// with 0x1 (or the compliment of 0x1) and "walks" the bit by shifting left for every new memory
/// location.
/// The procedure is repeated with offsets 0-31 or 0-63 depending on the size of `usize` to test all
/// bits in a memory location.
#[tracing::instrument(skip_all)]
pub fn test_mov_inv_walk<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    const NUM_RUNS: usize = size_of::<usize>();
    let num_runs_u64 = u64::try_from(NUM_RUNS).unwrap();
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul(MOV_INV_ITERATIONS * 2 * num_runs_u64))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    for i in 0..num_runs_u64 {
        let pattern = 1 << i;
        if let MemtestOutcome::Fail(f) = mov_inv_walking_pattern(memory, pattern, &mut observer)? {
            return Ok(MemtestOutcome::Fail(f));
        }
        if let MemtestOutcome::Fail(f) = mov_inv_walking_pattern(memory, !(pattern), &mut observer)?
        {
            return Ok(MemtestOutcome::Fail(f));
        }
    }
    Ok(MemtestOutcome::Pass)
}

fn mov_inv_walking_pattern<O: TestObserver>(
    memory: &mut [usize],
    starting_pattern: usize,
    observer: &mut O,
) -> MemtestResult<O> {
    let mut pattern = starting_pattern;
    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        write_volatile_safe(mem_ref, pattern);
        pattern = pattern.rotate_left(1);
    }

    pattern = starting_pattern;
    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        let address = address_from_ref(mem_ref);
        let actual = read_volatile_safe(mem_ref);

        if actual != pattern {
            info!("Test failed at 0x{address:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                address,
                expected: pattern,
                actual,
            }));
        }

        write_volatile_safe(mem_ref, !pattern);
        pattern = pattern.rotate_left(1);
    }

    pattern = !pattern;
    for mem_ref in memory.iter_mut().rev() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        pattern = pattern.rotate_right(1);
        let address = address_from_ref(mem_ref);
        let actual = read_volatile_safe(mem_ref);

        if actual != pattern {
            info!("Test failed at 0x{address:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                address,
                expected: pattern,
                actual,
            }));
        }

        write_volatile_safe(mem_ref, !pattern);
    }
    Ok(MemtestOutcome::Pass)
}

/// This test adapts the moving inversion algorithm implemented by [memtest86+](https://github.com/
/// memtest86plus/memtest86plus). For a detailed explanation of the algorithm, please refer
/// to the description available at the [Memtest86+ Test Algorithm Section](https://github.com/
/// memtest86plus/memtest86plus?tab=readme-ov-file#memtest86-test-algorithms)
///
/// This test runs the moving inversion algorithm with a random pattern for every memory location.
#[tracing::instrument(skip_all)]
pub fn test_mov_inv_random<O: TestObserver>(
    memory: &mut [usize],
    mut observer: O,
) -> MemtestResult<O> {
    use {
        rand::{rngs::SmallRng, Rng, SeedableRng},
        std::time::Instant,
    };
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul(MOV_INV_ITERATIONS * 2))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);
    let seed = {
        let time_bytes = Instant::now().elapsed().as_nanos().to_le_bytes();
        let mut seed = [0; 32];
        seed[0..16].copy_from_slice(&time_bytes);
        seed[16..32].copy_from_slice(&time_bytes);
        seed
    };

    let mut rng = SmallRng::from_seed(seed);
    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        write_volatile_safe(mem_ref, rng.gen());
    }

    let mut rng = SmallRng::from_seed(seed);
    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        let address = address_from_ref(mem_ref);
        let expected = rng.gen();
        let actual = read_volatile_safe(mem_ref);

        if actual != expected {
            info!("Test failed at 0x{address:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                address,
                expected,
                actual,
            }));
        }

        write_volatile_safe(mem_ref, !expected);
    }

    let mut rng = SmallRng::from_seed(seed);
    for mem_ref in memory.iter_mut() {
        observer.check().map_err(|e| MemtestError::Observer(e))?;
        let address = address_from_ref(mem_ref);
        let expected = !rng.gen::<usize>();
        let actual = read_volatile_safe(mem_ref);

        if actual != expected {
            info!("Test failed at 0x{address:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                address,
                expected,
                actual,
            }));
        }

        write_volatile_safe(mem_ref, !expected);
    }

    Ok(MemtestOutcome::Pass)
}

/// This test uses the Modulo-20 algorithm implemented by [memtest86+](https://github.com/
/// memtest86plus/memtest86plus),  which is designed to avoid effects of caching and buffering.
///
/// The test generates a random value, then write the value to every 20th memory location.
/// Afterwards write the complement of the value to all other locations. Then verify that the values
/// stored in every 20th location is unchanged.
/// The procedure is repeated with offsets 0-19 to test all memory locations.
#[tracing::instrument(skip_all)]
pub fn test_modulo_20<O: TestObserver>(memory: &mut [usize], mut observer: O) -> MemtestResult<O> {
    const STEP: usize = 20;
    (memory.len() > STEP)
        .then_some(())
        .context("Insufficient memory length for two-regions memtest")?;
    let expected_iter = u64::try_from(memory.len())
        .ok()
        .and_then(|count| count.checked_mul((STEP * 2).try_into().unwrap()))
        .context("Total number of iterations overflowed")?;
    observer.init(expected_iter);

    let pattern = random();
    for offset in 0..STEP {
        for mem_ref in memory.iter_mut().skip(offset).step_by(STEP) {
            observer.check().map_err(|e| MemtestError::Observer(e))?;
            write_volatile_safe(mem_ref, pattern);
        }

        for _ in 0..2 {
            for (i, mem_ref) in memory.iter_mut().enumerate() {
                if i % STEP == offset {
                    continue;
                }
                observer.check().map_err(|e| MemtestError::Observer(e))?;
                write_volatile_safe(mem_ref, !pattern);
            }
        }

        for mem_ref in memory.iter().skip(offset).step_by(STEP) {
            observer.check().map_err(|e| MemtestError::Observer(e))?;
            let address = address_from_ref(mem_ref);
            let expected = pattern;
            let actual = read_volatile_safe(mem_ref);

            if actual != expected {
                info!("Test failed at 0x{address:x}");
                return Ok(MemtestOutcome::Fail(MemtestFailure::UnexpectedValue {
                    address,
                    expected,
                    actual,
                }));
            }
        }
    }

    Ok(MemtestOutcome::Pass)
}

fn read_volatile_safe<T: Copy>(src: &T) -> T {
    unsafe { std::ptr::read_volatile(src) }
}

fn write_volatile_safe<T: Copy>(dst: &mut T, src: T) {
    unsafe { std::ptr::write_volatile(dst, src) }
}

fn split_slice_in_half(slice: &mut [usize]) -> anyhow::Result<(&mut [usize], &mut [usize])> {
    let mut it = slice.chunks_exact_mut(slice.len() / 2);
    let (Some(first), Some(second)) = (it.next(), it.next()) else {
        bail!("Insufficient memory length for two-regions memtest");
    };
    Ok((first, second))
}

fn mem_reset(memory: &mut [usize]) {
    for mem_ref in memory.iter_mut() {
        write_volatile_safe(mem_ref, !0);
    }
}

fn address_from_ref(r: &usize) -> usize {
    std::ptr::from_ref(r) as usize
}

/// Return a usize where all bytes are set to to value of `byte`
fn usize_filled_from_byte(byte: u8) -> usize {
    let mut val = 0;
    unsafe { std::ptr::write_bytes(&mut val, byte, 1) }
    val
}

fn compare_regions<O: TestObserver>(
    region1: &mut [usize],
    region2: &mut [usize],
    observer: &mut O,
) -> MemtestResult<O> {
    for (ref1, ref2) in region1.iter().zip(region2.iter()) {
        observer.check().map_err(|e| MemtestError::Observer(e))?;

        let address1 = address_from_ref(ref1);
        let address2 = address_from_ref(ref2);
        let val1 = read_volatile_safe(ref1);
        let val2 = read_volatile_safe(ref2);

        if val1 != val2 {
            info!("Test failed at 0x{address1:x} compared to 0x{address2:x}");
            return Ok(MemtestOutcome::Fail(MemtestFailure::MismatchedValues {
                address1,
                value1: val1,
                address2,
                value2: val2,
            }));
        }
    }
    Ok(MemtestOutcome::Pass)
}

impl fmt::Display for ParseMemtestKindError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for ParseMemtestKindError {}

impl fmt::Display for MemtestOutcome {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Outcome: {:?}", self)
    }
}

impl<E: Error> fmt::Display for MemtestError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Error: {:?}", self)
    }
}

impl<E: Error> Error for MemtestError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            MemtestError::Observer(_) => None,
            MemtestError::Other(err) => Some(err.as_ref()),
        }
    }
}

impl<E: Error> From<anyhow::Error> for MemtestError<E> {
    fn from(err: anyhow::Error) -> MemtestError<E> {
        MemtestError::Other(err)
    }
}

fn serialize_memtest_error_other<S>(error: &anyhow::Error, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format!("{:?}", error))
}

fn deserialize_memtest_error_other<'de, D>(deserializer: D) -> Result<anyhow::Error, D::Error>
where
    D: Deserializer<'de>,
{
    let str = String::deserialize(deserializer)?;
    Ok(anyhow!(str))
}

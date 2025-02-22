use std::fmt::Debug;
use std::marker::PhantomData;

use num::BigUint;
use plonky2_field::extension_field::{Extendable, FieldExtension};
use plonky2_field::field_types::{Field, PrimeField};

use crate::gadgets::arithmetic_u32::U32Target;
use crate::gadgets::biguint::BigUintTarget;
use crate::gadgets::nonnative::NonNativeTarget;
use crate::hash::hash_types::{HashOut, HashOutTarget, RichField};
use crate::iop::ext_target::ExtensionTarget;
use crate::iop::target::{BoolTarget, Target};
use crate::iop::wire::Wire;
use crate::iop::witness::{PartialWitness, PartitionWitness, Witness};
use crate::plonk::circuit_data::{CommonCircuitData, ProverOnlyCircuitData};
use crate::plonk::config::GenericConfig;

/// Given a `PartitionWitness` that has only inputs set, populates the rest of the witness using the
/// given set of generators.
pub(crate) fn generate_partial_witness<
    'a,
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    const D: usize,
>(
    inputs: PartialWitness<F>,
    prover_data: &'a ProverOnlyCircuitData<F, C, D>,
    common_data: &'a CommonCircuitData<F, C, D>,
) -> PartitionWitness<'a, F> {
    let config = &common_data.config;
    let generators = &prover_data.generators;
    let generator_indices_by_watches = &prover_data.generator_indices_by_watches;

    let mut witness = PartitionWitness::new(
        config.num_wires,
        common_data.degree(),
        common_data.num_virtual_targets,
        &prover_data.representative_map,
    );

    for (t, v) in inputs.target_values.into_iter() {
        witness.set_target(t, v);
    }

    // Build a list of "pending" generators which are queued to be run. Initially, all generators
    // are queued.
    let mut pending_generator_indices: Vec<_> = (0..generators.len()).collect();

    // We also track a list of "expired" generators which have already returned false.
    let mut generator_is_expired = vec![false; generators.len()];
    let mut remaining_generators = generators.len();

    let mut buffer = GeneratedValues::empty();

    // Keep running generators until we fail to make progress.
    while !pending_generator_indices.is_empty() {
        let mut next_pending_generator_indices = Vec::new();

        for &generator_idx in &pending_generator_indices {
            if generator_is_expired[generator_idx] {
                continue;
            }

            let finished = generators[generator_idx].run(&witness, &mut buffer);
            if finished {
                generator_is_expired[generator_idx] = true;
                remaining_generators -= 1;
            }

            // Merge any generated values into our witness, and get a list of newly-populated
            // targets' representatives.
            let new_target_reps = buffer
                .target_values
                .drain(..)
                .flat_map(|(t, v)| witness.set_target_returning_rep(t, v));

            // Enqueue unfinished generators that were watching one of the newly populated targets.
            for watch in new_target_reps {
                let opt_watchers = generator_indices_by_watches.get(&watch);
                if let Some(watchers) = opt_watchers {
                    for &watching_generator_idx in watchers {
                        if !generator_is_expired[watching_generator_idx] {
                            next_pending_generator_indices.push(watching_generator_idx);
                        }
                    }
                }
            }
        }

        pending_generator_indices = next_pending_generator_indices;
    }

    assert_eq!(
        remaining_generators, 0,
        "{} generators weren't run",
        remaining_generators,
    );

    witness
}

/// A generator participates in the generation of the witness.
pub trait WitnessGenerator<F: Field>: 'static + Send + Sync + Debug {
    /// Targets to be "watched" by this generator. Whenever a target in the watch list is populated,
    /// the generator will be queued to run.
    fn watch_list(&self) -> Vec<Target>;

    /// Run this generator, returning a flag indicating whether the generator is finished. If the
    /// flag is true, the generator will never be run again, otherwise it will be queued for another
    /// run next time a target in its watch list is populated.
    fn run(&self, witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>) -> bool;
}

/// Values generated by a generator invocation.
#[derive(Debug)]
pub struct GeneratedValues<F: Field> {
    pub(crate) target_values: Vec<(Target, F)>,
}

impl<F: Field> From<Vec<(Target, F)>> for GeneratedValues<F> {
    fn from(target_values: Vec<(Target, F)>) -> Self {
        Self { target_values }
    }
}

impl<F: Field> GeneratedValues<F> {
    pub fn with_capacity(capacity: usize) -> Self {
        Vec::with_capacity(capacity).into()
    }

    pub fn empty() -> Self {
        Vec::new().into()
    }

    pub fn singleton_wire(wire: Wire, value: F) -> Self {
        Self::singleton_target(Target::Wire(wire), value)
    }

    pub fn singleton_target(target: Target, value: F) -> Self {
        vec![(target, value)].into()
    }

    pub fn clear(&mut self) {
        self.target_values.clear();
    }

    pub fn singleton_extension_target<const D: usize>(
        et: ExtensionTarget<D>,
        value: F::Extension,
    ) -> Self
    where
        F: RichField + Extendable<D>,
    {
        let mut witness = Self::with_capacity(D);
        witness.set_extension_target(et, value);
        witness
    }

    pub fn set_target(&mut self, target: Target, value: F) {
        self.target_values.push((target, value))
    }

    pub fn set_bool_target(&mut self, target: BoolTarget, value: bool) {
        self.set_target(target.target, F::from_bool(value))
    }

    pub fn set_u32_target(&mut self, target: U32Target, value: u32) {
        self.set_target(target.0, F::from_canonical_u32(value))
    }

    pub fn set_biguint_target(&mut self, target: BigUintTarget, value: BigUint) {
        let mut limbs = value.to_u32_digits();

        assert!(target.num_limbs() >= limbs.len());

        limbs.resize(target.num_limbs(), 0);
        for i in 0..target.num_limbs() {
            self.set_u32_target(target.get_limb(i), limbs[i]);
        }
    }

    pub fn set_nonnative_target<FF: PrimeField>(&mut self, target: NonNativeTarget<FF>, value: FF) {
        self.set_biguint_target(target.value, value.to_canonical_biguint())
    }

    pub fn set_hash_target(&mut self, ht: HashOutTarget, value: HashOut<F>) {
        ht.elements
            .iter()
            .zip(value.elements)
            .for_each(|(&t, x)| self.set_target(t, x));
    }

    pub fn set_extension_target<const D: usize>(
        &mut self,
        et: ExtensionTarget<D>,
        value: F::Extension,
    ) where
        F: RichField + Extendable<D>,
    {
        let limbs = value.to_basefield_array();
        (0..D).for_each(|i| {
            self.set_target(et.0[i], limbs[i]);
        });
    }

    pub fn set_wire(&mut self, wire: Wire, value: F) {
        self.set_target(Target::Wire(wire), value)
    }

    pub fn set_wires<W>(&mut self, wires: W, values: &[F])
    where
        W: IntoIterator<Item = Wire>,
    {
        // If we used itertools, we could use zip_eq for extra safety.
        for (wire, &value) in wires.into_iter().zip(values) {
            self.set_wire(wire, value);
        }
    }

    pub fn set_ext_wires<W, const D: usize>(&mut self, wires: W, value: F::Extension)
    where
        F: RichField + Extendable<D>,
        W: IntoIterator<Item = Wire>,
    {
        self.set_wires(wires, &value.to_basefield_array());
    }
}

/// A generator which runs once after a list of dependencies is present in the witness.
pub trait SimpleGenerator<F: Field>: 'static + Send + Sync + Debug {
    fn dependencies(&self) -> Vec<Target>;

    fn run_once(&self, witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>);

    fn adapter(self) -> SimpleGeneratorAdapter<F, Self>
    where
        Self: Sized,
    {
        SimpleGeneratorAdapter {
            inner: self,
            _phantom: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct SimpleGeneratorAdapter<F: Field, SG: SimpleGenerator<F> + ?Sized> {
    _phantom: PhantomData<F>,
    inner: SG,
}

impl<F: Field, SG: SimpleGenerator<F>> WitnessGenerator<F> for SimpleGeneratorAdapter<F, SG> {
    fn watch_list(&self) -> Vec<Target> {
        self.inner.dependencies()
    }

    fn run(&self, witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>) -> bool {
        if witness.contains_all(&self.inner.dependencies()) {
            self.inner.run_once(witness, out_buffer);
            true
        } else {
            false
        }
    }
}

/// A generator which copies one wire to another.
#[derive(Debug)]
pub(crate) struct CopyGenerator {
    pub(crate) src: Target,
    pub(crate) dst: Target,
}

impl<F: Field> SimpleGenerator<F> for CopyGenerator {
    fn dependencies(&self) -> Vec<Target> {
        vec![self.src]
    }

    fn run_once(&self, witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>) {
        let value = witness.get_target(self.src);
        out_buffer.set_target(self.dst, value);
    }
}

/// A generator for including a random value
#[derive(Debug)]
pub(crate) struct RandomValueGenerator {
    pub(crate) target: Target,
}

impl<F: Field> SimpleGenerator<F> for RandomValueGenerator {
    fn dependencies(&self) -> Vec<Target> {
        Vec::new()
    }

    fn run_once(&self, _witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>) {
        let random_value = F::rand();

        out_buffer.set_target(self.target, random_value);
    }
}

/// A generator for testing if a value equals zero
#[derive(Debug)]
pub(crate) struct NonzeroTestGenerator {
    pub(crate) to_test: Target,
    pub(crate) dummy: Target,
}

impl<F: Field> SimpleGenerator<F> for NonzeroTestGenerator {
    fn dependencies(&self) -> Vec<Target> {
        vec![self.to_test]
    }

    fn run_once(&self, witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>) {
        let to_test_value = witness.get_target(self.to_test);

        let dummy_value = if to_test_value == F::ZERO {
            F::ONE
        } else {
            to_test_value.inverse()
        };

        out_buffer.set_target(self.dummy, dummy_value);
    }
}

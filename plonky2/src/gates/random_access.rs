use std::marker::PhantomData;

use itertools::Itertools;
use plonky2_field::extension_field::Extendable;
use plonky2_field::field_types::Field;
use plonky2_field::packed_field::PackedField;

use crate::gates::gate::Gate;
use crate::gates::packed_util::PackedEvaluableBase;
use crate::gates::util::StridedConstraintConsumer;
use crate::hash::hash_types::RichField;
use crate::iop::ext_target::ExtensionTarget;
use crate::iop::generator::{GeneratedValues, SimpleGenerator, WitnessGenerator};
use crate::iop::target::Target;
use crate::iop::wire::Wire;
use crate::iop::witness::{PartitionWitness, Witness};
use crate::plonk::circuit_builder::CircuitBuilder;
use crate::plonk::circuit_data::CircuitConfig;
use crate::plonk::vars::{
    EvaluationTargets, EvaluationVars, EvaluationVarsBase, EvaluationVarsBaseBatch,
    EvaluationVarsBasePacked,
};

/// A gate for checking that a particular element of a list matches a given value.
#[derive(Copy, Clone, Debug)]
pub(crate) struct RandomAccessGate<F: RichField + Extendable<D>, const D: usize> {
    pub bits: usize,
    pub num_copies: usize,
    _phantom: PhantomData<F>,
}

impl<F: RichField + Extendable<D>, const D: usize> RandomAccessGate<F, D> {
    fn new(num_copies: usize, bits: usize) -> Self {
        Self {
            bits,
            num_copies,
            _phantom: PhantomData,
        }
    }

    pub fn new_from_config(config: &CircuitConfig, bits: usize) -> Self {
        let vec_size = 1 << bits;
        // Need `(2 + vec_size) * num_copies` routed wires
        let max_copies = (config.num_routed_wires / (2 + vec_size)).min(
            // Need `(2 + vec_size + bits) * num_copies` wires
            config.num_wires / (2 + vec_size + bits),
        );
        Self::new(max_copies, bits)
    }

    fn vec_size(&self) -> usize {
        1 << self.bits
    }

    pub fn wire_access_index(&self, copy: usize) -> usize {
        debug_assert!(copy < self.num_copies);
        (2 + self.vec_size()) * copy
    }

    pub fn wire_claimed_element(&self, copy: usize) -> usize {
        debug_assert!(copy < self.num_copies);
        (2 + self.vec_size()) * copy + 1
    }

    pub fn wire_list_item(&self, i: usize, copy: usize) -> usize {
        debug_assert!(i < self.vec_size());
        debug_assert!(copy < self.num_copies);
        (2 + self.vec_size()) * copy + 2 + i
    }

    fn start_of_intermediate_wires(&self) -> usize {
        (2 + self.vec_size()) * self.num_copies
    }

    pub(crate) fn num_routed_wires(&self) -> usize {
        self.start_of_intermediate_wires()
    }

    /// An intermediate wire where the prover gives the (purported) binary decomposition of the
    /// index.
    pub fn wire_bit(&self, i: usize, copy: usize) -> usize {
        debug_assert!(i < self.bits);
        debug_assert!(copy < self.num_copies);
        self.start_of_intermediate_wires() + copy * self.bits + i
    }
}

impl<F: RichField + Extendable<D>, const D: usize> Gate<F, D> for RandomAccessGate<F, D> {
    fn id(&self) -> String {
        format!("{:?}<D={}>", self, D)
    }

    fn eval_unfiltered(&self, vars: EvaluationVars<F, D>) -> Vec<F::Extension> {
        let mut constraints = Vec::with_capacity(self.num_constraints());

        for copy in 0..self.num_copies {
            let access_index = vars.local_wires[self.wire_access_index(copy)];
            let mut list_items = (0..self.vec_size())
                .map(|i| vars.local_wires[self.wire_list_item(i, copy)])
                .collect::<Vec<_>>();
            let claimed_element = vars.local_wires[self.wire_claimed_element(copy)];
            let bits = (0..self.bits)
                .map(|i| vars.local_wires[self.wire_bit(i, copy)])
                .collect::<Vec<_>>();

            // Assert that each bit wire value is indeed boolean.
            for &b in &bits {
                constraints.push(b * (b - F::Extension::ONE));
            }

            // Assert that the binary decomposition was correct.
            let reconstructed_index = bits
                .iter()
                .rev()
                .fold(F::Extension::ZERO, |acc, &b| acc.double() + b);
            constraints.push(reconstructed_index - access_index);

            // Repeatedly fold the list, selecting the left or right item from each pair based on
            // the corresponding bit.
            for b in bits {
                list_items = list_items
                    .iter()
                    .tuples()
                    .map(|(&x, &y)| x + b * (y - x))
                    .collect()
            }

            debug_assert_eq!(list_items.len(), 1);
            constraints.push(list_items[0] - claimed_element);
        }

        constraints
    }

    fn eval_unfiltered_base_one(
        &self,
        _vars: EvaluationVarsBase<F>,
        _yield_constr: StridedConstraintConsumer<F>,
    ) {
        panic!("use eval_unfiltered_base_packed instead");
    }

    fn eval_unfiltered_base_batch(&self, vars_base: EvaluationVarsBaseBatch<F>) -> Vec<F> {
        self.eval_unfiltered_base_batch_packed(vars_base)
    }

    fn eval_unfiltered_recursively(
        &self,
        builder: &mut CircuitBuilder<F, D>,
        vars: EvaluationTargets<D>,
    ) -> Vec<ExtensionTarget<D>> {
        let zero = builder.zero_extension();
        let two = builder.two_extension();
        let mut constraints = Vec::with_capacity(self.num_constraints());

        for copy in 0..self.num_copies {
            let access_index = vars.local_wires[self.wire_access_index(copy)];
            let mut list_items = (0..self.vec_size())
                .map(|i| vars.local_wires[self.wire_list_item(i, copy)])
                .collect::<Vec<_>>();
            let claimed_element = vars.local_wires[self.wire_claimed_element(copy)];
            let bits = (0..self.bits)
                .map(|i| vars.local_wires[self.wire_bit(i, copy)])
                .collect::<Vec<_>>();

            // Assert that each bit wire value is indeed boolean.
            for &b in &bits {
                constraints.push(builder.mul_sub_extension(b, b, b));
            }

            // Assert that the binary decomposition was correct.
            let reconstructed_index = bits
                .iter()
                .rev()
                .fold(zero, |acc, &b| builder.mul_add_extension(acc, two, b));
            constraints.push(builder.sub_extension(reconstructed_index, access_index));

            // Repeatedly fold the list, selecting the left or right item from each pair based on
            // the corresponding bit.
            for b in bits {
                list_items = list_items
                    .iter()
                    .tuples()
                    .map(|(&x, &y)| builder.select_ext_generalized(b, y, x))
                    .collect()
            }

            debug_assert_eq!(list_items.len(), 1);
            constraints.push(builder.sub_extension(list_items[0], claimed_element));
        }

        constraints
    }

    fn generators(
        &self,
        gate_index: usize,
        _local_constants: &[F],
    ) -> Vec<Box<dyn WitnessGenerator<F>>> {
        (0..self.num_copies)
            .map(|copy| {
                let g: Box<dyn WitnessGenerator<F>> = Box::new(
                    RandomAccessGenerator {
                        gate_index,
                        gate: *self,
                        copy,
                    }
                    .adapter(),
                );
                g
            })
            .collect()
    }

    fn num_wires(&self) -> usize {
        self.wire_bit(self.bits - 1, self.num_copies - 1) + 1
    }

    fn num_constants(&self) -> usize {
        0
    }

    fn degree(&self) -> usize {
        self.bits + 1
    }

    fn num_constraints(&self) -> usize {
        let constraints_per_copy = self.bits + 2;
        self.num_copies * constraints_per_copy
    }
}

impl<F: RichField + Extendable<D>, const D: usize> PackedEvaluableBase<F, D>
    for RandomAccessGate<F, D>
{
    fn eval_unfiltered_base_packed<P: PackedField<Scalar = F>>(
        &self,
        vars: EvaluationVarsBasePacked<P>,
        mut yield_constr: StridedConstraintConsumer<P>,
    ) {
        for copy in 0..self.num_copies {
            let access_index = vars.local_wires[self.wire_access_index(copy)];
            let mut list_items = (0..self.vec_size())
                .map(|i| vars.local_wires[self.wire_list_item(i, copy)])
                .collect::<Vec<_>>();
            let claimed_element = vars.local_wires[self.wire_claimed_element(copy)];
            let bits = (0..self.bits)
                .map(|i| vars.local_wires[self.wire_bit(i, copy)])
                .collect::<Vec<_>>();

            // Assert that each bit wire value is indeed boolean.
            for &b in &bits {
                yield_constr.one(b * (b - F::ONE));
            }

            // Assert that the binary decomposition was correct.
            let reconstructed_index = bits.iter().rev().fold(P::ZEROS, |acc, &b| acc + acc + b);
            yield_constr.one(reconstructed_index - access_index);

            // Repeatedly fold the list, selecting the left or right item from each pair based on
            // the corresponding bit.
            for b in bits {
                list_items = list_items
                    .iter()
                    .tuples()
                    .map(|(&x, &y)| x + b * (y - x))
                    .collect()
            }

            debug_assert_eq!(list_items.len(), 1);
            yield_constr.one(list_items[0] - claimed_element);
        }
    }
}

#[derive(Debug)]
struct RandomAccessGenerator<F: RichField + Extendable<D>, const D: usize> {
    gate_index: usize,
    gate: RandomAccessGate<F, D>,
    copy: usize,
}

impl<F: RichField + Extendable<D>, const D: usize> SimpleGenerator<F>
    for RandomAccessGenerator<F, D>
{
    fn dependencies(&self) -> Vec<Target> {
        let local_target = |input| Target::wire(self.gate_index, input);

        let mut deps = vec![local_target(self.gate.wire_access_index(self.copy))];
        for i in 0..self.gate.vec_size() {
            deps.push(local_target(self.gate.wire_list_item(i, self.copy)));
        }
        deps
    }

    fn run_once(&self, witness: &PartitionWitness<F>, out_buffer: &mut GeneratedValues<F>) {
        let local_wire = |input| Wire {
            gate: self.gate_index,
            input,
        };

        let get_local_wire = |input| witness.get_wire(local_wire(input));
        let mut set_local_wire = |input, value| out_buffer.set_wire(local_wire(input), value);

        let copy = self.copy;
        let vec_size = self.gate.vec_size();

        let access_index_f = get_local_wire(self.gate.wire_access_index(copy));
        let access_index = access_index_f.to_canonical_u64() as usize;
        debug_assert!(
            access_index < vec_size,
            "Access index {} is larger than the vector size {}",
            access_index,
            vec_size
        );

        set_local_wire(
            self.gate.wire_claimed_element(copy),
            get_local_wire(self.gate.wire_list_item(access_index, copy)),
        );

        for i in 0..self.gate.bits {
            let bit = F::from_bool(((access_index >> i) & 1) != 0);
            set_local_wire(self.gate.wire_bit(i, copy), bit);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;

    use anyhow::Result;
    use plonky2_field::field_types::Field;
    use plonky2_field::goldilocks_field::GoldilocksField;
    use rand::{thread_rng, Rng};

    use crate::gates::gate::Gate;
    use crate::gates::gate_testing::{test_eval_fns, test_low_degree};
    use crate::gates::random_access::RandomAccessGate;
    use crate::hash::hash_types::HashOut;
    use crate::plonk::config::{GenericConfig, PoseidonGoldilocksConfig};
    use crate::plonk::vars::EvaluationVars;

    #[test]
    fn low_degree() {
        test_low_degree::<GoldilocksField, _, 4>(RandomAccessGate::new(4, 4));
    }

    #[test]
    fn eval_fns() -> Result<()> {
        const D: usize = 2;
        type C = PoseidonGoldilocksConfig;
        type F = <C as GenericConfig<D>>::F;
        test_eval_fns::<F, C, _, D>(RandomAccessGate::new(4, 4))
    }

    #[test]
    fn test_gate_constraint() {
        const D: usize = 2;
        type C = PoseidonGoldilocksConfig;
        type F = <C as GenericConfig<D>>::F;
        type FF = <C as GenericConfig<D>>::FE;

        /// Returns the local wires for a random access gate given the vectors, elements to compare,
        /// and indices.
        fn get_wires(
            bits: usize,
            lists: Vec<Vec<F>>,
            access_indices: Vec<usize>,
            claimed_elements: Vec<F>,
        ) -> Vec<FF> {
            let num_copies = lists.len();
            let vec_size = lists[0].len();

            let mut v = Vec::new();
            let mut bit_vals = Vec::new();
            for copy in 0..num_copies {
                let access_index = access_indices[copy];
                v.push(F::from_canonical_usize(access_index));
                v.push(claimed_elements[copy]);
                for j in 0..vec_size {
                    v.push(lists[copy][j]);
                }

                for i in 0..bits {
                    bit_vals.push(F::from_bool(((access_index >> i) & 1) != 0));
                }
            }
            v.extend(bit_vals);

            v.iter().map(|&x| x.into()).collect()
        }

        let bits = 3;
        let vec_size = 1 << bits;
        let num_copies = 4;
        let lists = (0..num_copies)
            .map(|_| F::rand_vec(vec_size))
            .collect::<Vec<_>>();
        let access_indices = (0..num_copies)
            .map(|_| thread_rng().gen_range(0..vec_size))
            .collect::<Vec<_>>();
        let gate = RandomAccessGate::<F, D> {
            bits,
            num_copies,
            _phantom: PhantomData,
        };

        let good_claimed_elements = lists
            .iter()
            .zip(&access_indices)
            .map(|(l, &i)| l[i])
            .collect();
        let good_vars = EvaluationVars {
            local_constants: &[],
            local_wires: &get_wires(
                bits,
                lists.clone(),
                access_indices.clone(),
                good_claimed_elements,
            ),
            public_inputs_hash: &HashOut::rand(),
        };
        let bad_claimed_elements = F::rand_vec(4);
        let bad_vars = EvaluationVars {
            local_constants: &[],
            local_wires: &get_wires(bits, lists, access_indices, bad_claimed_elements),
            public_inputs_hash: &HashOut::rand(),
        };

        assert!(
            gate.eval_unfiltered(good_vars).iter().all(|x| x.is_zero()),
            "Gate constraints are not satisfied."
        );
        assert!(
            !gate.eval_unfiltered(bad_vars).iter().all(|x| x.is_zero()),
            "Gate constraints are satisfied but should not be."
        );
    }
}

#[cfg(feature = "std")]
use std::error::Error;

use num::Float;

use alloc::{collections::BinaryHeap, vec, vec::Vec};
use core::{borrow::Borrow, cmp::Reverse, convert::Infallible, marker::PhantomData, ops::Add};

use super::{
    Codebook, DecoderCodebook, DecodingError, EncoderCodebook, SmallBitVec,
    SmallBitVecReverseIterator,
};
use crate::{BitArray, EncoderError, EncoderFrontendError, UnwrapInfallible};

/// The type parameter `W` is used in the implementation of [`EncoderCodebook`],
/// which temporarily builds up the reversed codeword in a `SmallBitVec<W>` before
/// reversing it back to its intended order. The default `W = usize` is usually a
/// good choice.
#[derive(Debug, Clone)]
pub struct EncoderHuffmanTree<W: BitArray = usize> {
    /// A `Vec` of size `num_symbols * 2 - 1`, where the first `num_symbol` items
    /// correspond to the symbols, i.e., leaf nodes of the Huffman tree, and the
    /// remaining items are ancestors. An entry with value `x: usize` represents a node
    /// with the following properties:
    /// - root node if `x == 0`;
    /// - otherwise, the lowest significant bit distinguishes left vs right children,
    ///   and the parent node is at index `x >> 1`.
    /// (This works the node with index 0, if it exists, is always a leaf node, i.e., it
    /// cannot be any other node's parent node.)
    ///
    /// It is guaranteed that `num_symbols != 0` i.e., `nodes` is not empty.
    nodes: Vec<usize>,

    phantom: PhantomData<W>,
}

impl EncoderHuffmanTree {
    pub fn from_probabilities<P, I>(probabilities: I) -> Self
    where
        P: Ord + Clone + Add<Output = P>,
        I: IntoIterator,
        I::Item: Borrow<P>,
    {
        Self::try_from_probabilities::<_, Infallible, _>(
            probabilities.into_iter().map(|p| Ok(p.borrow().clone())),
        )
        .unwrap_infallible()
    }

    pub fn from_float_probabilities<P, I>(probabilities: I) -> Result<Self, ()>
    where
        P: Float + Clone + Add<Output = P>,
        I: IntoIterator,
        I::Item: Borrow<P>,
    {
        Self::try_from_probabilities(
            probabilities
                .into_iter()
                .map(|p| NonNanFloat::new(p.borrow().clone())),
        )
    }

    pub fn try_from_probabilities<P, E, I>(probabilities: I) -> Result<Self, E>
    where
        P: Ord + Clone + Add<Output = P>,
        I: IntoIterator<Item = Result<P, E>>,
    {
        let mut heap = probabilities
            .into_iter()
            .enumerate()
            .map(|(i, s)| s.map(|s| (Reverse((s, i)))))
            .collect::<Result<BinaryHeap<_>, E>>()?;

        if heap.is_empty() || heap.len() > usize::max_value() / 4 {
            panic!();
        }

        let mut nodes = vec![0; heap.len() * 2 - 1];
        let mut next_node_index = heap.len();

        while let (Some(Reverse((prob0, index0))), Some(Reverse((prob1, index1)))) =
            (heap.pop(), heap.pop())
        {
            heap.push(Reverse((prob0 + prob1, next_node_index)));
            unsafe {
                // SAFETY:
                // - `nodes.len() == original_heap_len * 2 - 1` (which we made sure doesn't wrap),
                //   where `original_heap_len` is the value of `heap.len()` before entering this
                //   `while` loop, which we checked is nonzero.
                // - We access `nodes` and indices found in `heap`. These have to be either the
                //   indices `0..original_heap_len` that we wrote into it initially (which are all
                //   smaller than `original_heap_len * 2 - 1` since we checked that
                //   `!heap.is_empty()`, i.e., that `original_heap_len != 0`); or they have to be
                //   the indices we write to the heap in this `while` loop, which come from
                //   `next_node_index`.
                // - `next_node_index` starts at `original_heap_len` and increases by one in each
                //   iteration of this `while` loop.
                // - Each iteration of this `while` loop removes two elements from `heap` and
                //   pushes one element back onto `heap`; so each iteration reduces the number of
                //   elements on `heap` by one; since we terminate as soon as there are fewer than
                //   2 elements on `heap`, this `while` loop iterates `original_heap_len - 1` times
                //   (which is nonnegative since `original_heap_len != 0`).
                // - Thus, the largest value that `next_node_index` can take is
                //   `original_heap_len * 2 - 1`; but since we access `next_node_index` before
                //   incrementing it, all values we ever push on the heap are strictly smaller than
                //   `original_heap_len * 2 - 1`, and thus are valid indices.
                *nodes.get_unchecked_mut(index0) = next_node_index << 1;
                *nodes.get_unchecked_mut(index1) = (next_node_index << 1) | 1;
            }
            next_node_index += 1;
        }

        Ok(Self {
            nodes,
            phantom: PhantomData,
        })
    }
}

impl<W: BitArray> Codebook for EncoderHuffmanTree<W> {
    fn num_symbols(&self) -> usize {
        self.nodes.len() / 2 + 1
    }
}

impl<W: BitArray> EncoderCodebook for EncoderHuffmanTree<W> {
    type BitIterator = SmallBitVecReverseIterator<W>;

    fn encode_symbol(&self, symbol: usize) -> Result<Self::BitIterator, EncoderError<Infallible>> {
        if symbol > self.nodes.len() / 2 {
            return Err(EncoderFrontendError::ImpossibleSymbol.into_encoder_error());
        }

        let mut reverse_codeword = SmallBitVec::<W>::new();
        let mut node_index = symbol;
        loop {
            let node = unsafe {
                // SAFETY: `node_index` is
                // - either its initial value of `symbol`, which is `<= num_symbols`, and
                //   `nodes.len() = 2 * num_symbols - 1 > num_symbols` since `num_symbols != 0`;
                // - or `node_index` is `node >> 1` where `node` is the value of a parent node; in
                //   this case it is guaranteed to be a valid index.
                *self.nodes.get_unchecked(node_index)
            };
            if node == 0 {
                break;
            }
            reverse_codeword.push(node & 1 != 0);
            node_index = node >> 1;
        }

        Ok(reverse_codeword.into_iter_reverse())
    }
}

#[derive(Debug, Clone)]
pub struct DecoderHuffmanTree {
    /// A `Vec` of size `num_symbols - 1`, containing only the non-leaf nodes of the
    /// Huffman tree. The root node is at the end. An entry with value
    /// `[x, y]: [usize; 2]` represents a with children `x` and `y`, each represented
    /// either by the associated symbol (if the respective child is a leaf node), or by
    /// `num_symbols + index` where `index` is the index into `nodes` where the
    /// respective child node can be found.
    ///
    /// # Invariants
    /// - `num_symbols != 0` (but `nodes` can still be empty if `num_symbols == 1`.
    /// - All entries of `nodes` are strictly smaller than `2 * nodes.len()`.
    nodes: Vec<[usize; 2]>,
}

impl DecoderHuffmanTree {
    pub fn from_probabilities<P, I>(probabilities: I) -> Self
    where
        P: Ord + Clone + Add<Output = P>,
        I: IntoIterator,
        I::Item: Borrow<P>,
    {
        Self::try_from_probabilities::<_, Infallible, _>(
            probabilities.into_iter().map(|p| Ok(p.borrow().clone())),
        )
        .unwrap_infallible()
    }

    pub fn from_float_probabilities<P, I>(probabilities: I) -> Result<Self, ()>
    where
        P: Float + Clone + Add<Output = P>,
        I: IntoIterator,
        I::Item: Borrow<P>,
    {
        Self::try_from_probabilities(
            probabilities
                .into_iter()
                .map(|p| NonNanFloat::new(p.borrow().clone())),
        )
    }

    pub fn try_from_probabilities<P, E, I>(probabilities: I) -> Result<Self, E>
    where
        P: Ord + Clone + Add<Output = P>,
        I: IntoIterator<Item = Result<P, E>>,
    {
        let mut heap = probabilities
            .into_iter()
            .enumerate()
            .map(|(i, s)| s.map(|s| (Reverse((s, i)))))
            .collect::<Result<BinaryHeap<_>, E>>()?;

        if heap.is_empty() || heap.len() > usize::max_value() / 2 {
            panic!();
        }

        let mut nodes = Vec::with_capacity(heap.len() - 1);
        let mut next_node_index = heap.len();

        while let (Some(Reverse((prob0, index0))), Some(Reverse((prob1, index1)))) =
            (heap.pop(), heap.pop())
        {
            heap.push(Reverse((prob0 + prob1, next_node_index)));
            nodes.push([index0, index1]);
            next_node_index += 1;
        }

        Ok(Self { nodes })
    }
}

impl Codebook for DecoderHuffmanTree {
    fn num_symbols(&self) -> usize {
        self.nodes.len() + 1
    }
}

impl DecoderCodebook for DecoderHuffmanTree {
    fn decode_symbol(
        &self,
        mut source: impl Iterator<Item = bool>,
    ) -> Result<usize, DecodingError> {
        let num_nodes = self.nodes.len();
        let num_symbols = num_nodes + 1;
        let mut node_index = 2 * num_nodes; // Start at root node.

        while node_index >= num_symbols {
            let bit = source.next().ok_or(DecodingError::OutOfCompressedData)?;
            unsafe {
                // SAFETY:
                // - `node_index >= num_symbols` within this loop, so `node_index - num_symbols`
                //   does not wrap.
                // - `node_index is either the initial value `2 * num_nodes` or it comes from an
                //   entry of `nodes`, which are all strictly smaller than `2 * num_nodes`.
                // - Thus, `node_index - num_symbols = node_index - num_nodes - 1 <= num_nodes - 1`,
                //   which is a valid index into `nodes`.
                //
                // NOTE: No need to use `get_unchecked(bit as usize)` since the compiler is smart
                //       enough to optimize away the bounds check in this case on its own.
                node_index = self.nodes.get_unchecked(node_index - num_symbols)[bit as usize];
            }
        }

        Ok(node_index)
    }
}

pub trait TryIntoOrd {
    type Target: Ord;

    #[cfg(not(feature = "std"))]
    type Error: Debug;

    #[cfg(feature = "std")]
    type Error: Error;

    fn try_into(self) -> Result<Self::Target, Self::Error>;
}

#[derive(PartialOrd, Clone, Copy)]
struct NonNanFloat<F: Float> {
    inner: F,
}

impl<F: Float> NonNanFloat<F> {
    fn new(x: F) -> Result<Self, ()> {
        if x.is_nan() {
            Err(())
        } else {
            Ok(Self { inner: x })
        }
    }
}

impl<F: Float> PartialEq for NonNanFloat<F> {
    fn eq(&self, other: &Self) -> bool {
        self.inner.eq(&other.inner)
    }
}

impl<F: Float> Eq for NonNanFloat<F> {}

impl<F: Float> Ord for NonNanFloat<F> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.inner
            .partial_cmp(&other.inner)
            .expect("NonNanFloat::inner is not NaN.")
    }
}

impl<F: Float> Add for NonNanFloat<F> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        NonNanFloat {
            inner: self.inner + rhs.inner,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    extern crate std;
    use std::string::String;

    #[test]
    fn encoder_huffman_tree() {
        fn encode_all_symbols<W: BitArray>(tree: &EncoderHuffmanTree<W>) -> Vec<String> {
            (0..tree.num_symbols())
                .map(|symbol| {
                    tree.encode_symbol(symbol)
                        .unwrap()
                        .map(|bit| if bit { '1' } else { '0' })
                        .collect::<String>()
                })
                .collect()
        }

        let tree = EncoderHuffmanTree::from_probabilities::<u32, _>(&[1]);
        assert_eq!(tree.nodes, [0]);
        assert_eq!(encode_all_symbols(&tree), [""]);

        let tree = EncoderHuffmanTree::from_probabilities::<u32, _>(&[1, 2]);
        assert_eq!(tree.nodes, [4, 5, 0]);
        assert_eq!(encode_all_symbols(&tree), ["0", "1"]);

        let tree = EncoderHuffmanTree::from_probabilities::<u32, _>(&[2, 1]);
        assert_eq!(tree.nodes, [5, 4, 0]);
        assert_eq!(encode_all_symbols(&tree), ["1", "0"]);

        // Ties are broken by index.
        let tree = EncoderHuffmanTree::from_probabilities::<u32, _>(&[1, 1]);
        assert_eq!(tree.nodes, [4, 5, 0]);
        assert_eq!(encode_all_symbols(&tree), ["0", "1"]);

        let tree = EncoderHuffmanTree::from_probabilities::<u32, _>(&[2, 2, 4, 1, 1]);
        assert_eq!(tree.nodes, [12, 13, 15, 10, 11, 14, 16, 17, 0]);
        assert_eq!(encode_all_symbols(&tree), ["00", "01", "11", "100", "101"]);

        // Let's not test ties of sums in floating point probabilities since they'll depend
        // on rounding errors (but should still be deterministic).
        let tree =
            EncoderHuffmanTree::from_float_probabilities::<f32, _>(&[0.19, 0.2, 0.41, 0.1, 0.1])
                .unwrap();
        assert_eq!(tree.nodes, [12, 13, 16, 10, 11, 14, 15, 17, 0,]);
        assert_eq!(
            encode_all_symbols(&tree),
            ["110", "111", "0", "100", "101",]
        );
    }

    #[test]
    fn decoder_huffman_tree() {
        fn test_decoding_all_symbols<W: BitArray>(
            decoder_tree: &DecoderHuffmanTree,
            encoder_tree: &EncoderHuffmanTree<W>,
        ) {
            for symbol in 0..encoder_tree.num_symbols() {
                let mut codeword = encoder_tree.encode_symbol(symbol).unwrap();
                let decoded = decoder_tree.decode_symbol(&mut codeword).unwrap();
                assert_eq!(symbol, decoded);
                assert!(codeword.next().is_none());
            }
        }

        let tree = DecoderHuffmanTree::from_probabilities::<u32, _>(&[1]);
        assert!(tree.nodes.is_empty());
        test_decoding_all_symbols(
            &tree,
            &EncoderHuffmanTree::from_probabilities::<u32, _>(&[1]),
        );

        let tree = DecoderHuffmanTree::from_probabilities::<u32, _>(&[1, 2]);
        assert_eq!(tree.nodes, [[0, 1]]);
        test_decoding_all_symbols(
            &tree,
            &EncoderHuffmanTree::from_probabilities::<u32, _>(&[0, 1]),
        );

        let tree = DecoderHuffmanTree::from_probabilities::<u32, _>(&[2, 1]);
        assert_eq!(tree.nodes, [[1, 0]]);
        test_decoding_all_symbols(
            &tree,
            &EncoderHuffmanTree::from_probabilities::<u32, _>(&[2, 1]),
        );

        // Ties are broken by index.
        let tree = DecoderHuffmanTree::from_probabilities::<u32, _>(&[1u32, 1]);
        assert_eq!(tree.nodes, [[0, 1]]);
        test_decoding_all_symbols(
            &tree,
            &EncoderHuffmanTree::from_probabilities::<u32, _>(&[1, 1]),
        );

        let tree = DecoderHuffmanTree::from_probabilities::<u32, _>(&[2, 2, 4, 1, 1]);
        assert_eq!(tree.nodes, [[3, 4], [0, 1], [5, 2], [6, 7]]);
        test_decoding_all_symbols(
            &tree,
            &EncoderHuffmanTree::from_probabilities::<u32, _>(&[2, 2, 4, 1, 1]),
        );

        // Let's not test ties of sums in floating point probabilities since they'll depend
        // on rounding errors (but should still be deterministic).
        let tree =
            DecoderHuffmanTree::from_float_probabilities::<f32, _>(&[0.19, 0.2, 0.41, 0.1, 0.1])
                .unwrap();
        assert_eq!(tree.nodes, [[3, 4], [0, 1], [5, 6], [2, 7]]);
        test_decoding_all_symbols(
            &tree,
            &EncoderHuffmanTree::from_float_probabilities::<f32, _>(&[0.19, 0.2, 0.41, 0.1, 0.1])
                .unwrap(),
        );
    }
}

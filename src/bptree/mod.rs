//! See the documentation for `BptreeMap`
#[macro_use]
mod macros;
mod cursor;
pub mod iter;
mod node;
mod states;

use self::cursor::CursorReadOps;
use self::cursor::{CursorRead, CursorWrite, SuperBlock};
use self::iter::{Iter, KeyIter, ValueIter};
// use self::node::{Leaf, Node};
use parking_lot::{Mutex, MutexGuard};
use std::borrow::Borrow;
use std::fmt::Debug;
use std::iter::FromIterator;
// use std::marker::PhantomData;
use std::sync::Arc;

/// A concurrently readable map based on a modified B+Tree structure.
///
/// This structure can be used in locations where you would otherwise us
/// `RwLock<BTreeMap>` or `Mutex<BTreeMap>`.
///
/// Generally, the concurrent HashMap is a better choice unless you require
/// ordered key storage.
///
/// This is a concurrently readable structure, meaning it has transactional
/// properties. Writers are serialised (one after the other), and readers
/// can exist in parallel with stable views of the structure at a point
/// in time.
///
/// This is achieved through the use of COW or MVCC. As a write occurs
/// subsets of the tree are cloned into the writer thread and then commited
/// later. This may cause memory usage to increase in exchange for a gain
/// in concurrent behaviour.
///
/// Transactions can be rolled-back (aborted) without penalty by dropping
/// the `BptreeMapWriteTxn` without calling `commit()`.
pub struct BptreeMap<K, V>
where
    K: Ord + Clone + Debug,
    V: Clone,
{
    write: Mutex<()>,
    active: Mutex<Arc<SuperBlock<K, V>>>,
}

unsafe impl<K: Clone + Ord + Debug + Send + 'static, V: Clone + Send + 'static> Send for BptreeMap<K, V> {}
unsafe impl<K: Clone + Ord + Debug + Send + 'static, V: Clone + Send + 'static> Sync for BptreeMap<K, V> {}

/// An active read transaction over a `BptreeMap`. The data in this tree
/// is guaranteed to not change and will remain consistent for the life
/// of this transaction.
pub struct BptreeMapReadTxn<'a, K, V>
where
    K: Ord + Clone + Debug,
    V: Clone,
{
    _caller: &'a BptreeMap<K, V>,
    pin: Arc<SuperBlock<K, V>>,
    work: CursorRead<K, V>,
}

/// An active write transaction for a `BptreeMap`. The data in this tree
/// may be modified exclusively through this transaction without affecting
/// readers. The write may be rolledback/aborted by dropping this guard
/// without calling `commit()`. Once `commit()` is called, readers will be
/// able to access and percieve changes in new transactions.
pub struct BptreeMapWriteTxn<'a, K, V>
where
    K: Ord + Clone + Debug,
    V: Clone,
{
    work: CursorWrite<K, V>,
    caller: &'a BptreeMap<K, V>,
    _guard: MutexGuard<'a, ()>,
}

enum SnapshotType<'a, K, V>
where
    K: Ord + Clone + Debug,
    V: Clone,
{
    R(&'a CursorRead<K, V>),
    W(&'a CursorWrite<K, V>),
}

/// A point-in-time snapshot of the tree from within a read OR write. This is
/// useful for building other transactional types ontop of this structure, as
/// you need a way to downcast both BptreeMapReadTxn or BptreeMapWriteTxn to
/// a singular reader type for a number of get_inner() style patterns.
///
/// This snapshot IS safe within the read thread due to the nature of the
/// implementation borrowing the inner tree to prevent mutations within the
/// same thread while the read snapshot is open.
pub struct BptreeMapReadSnapshot<'a, K, V>
where
    K: Ord + Clone + Debug,
    V: Clone,
{
    work: SnapshotType<'a, K, V>,
}

impl<K: Clone + Ord + Debug, V: Clone> Default for BptreeMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Clone + Ord + Debug, V: Clone> BptreeMap<K, V> {
    /// Construct a new concurrent tree
    pub fn new() -> Self {
        BptreeMap {
            write: Mutex::new(()),
            active: Mutex::new(Arc::new(SuperBlock::default())),
        }
    }

    /// Initiate a read transaction for the tree, concurrent to any
    /// other readers or writers.
    pub fn read<'a>(&'a self) -> BptreeMapReadTxn<'a, K, V> {
        let rguard = self.active.lock();
        let pin = rguard.clone();
        let work = CursorRead::new(pin.as_ref());
        BptreeMapReadTxn {
            _caller: self,
            pin,
            work,
        }
    }

    /// Initiate a write transaction for the tree, exclusive to this
    /// writer, and concurrently to all existing reads.
    pub fn write(&self) -> BptreeMapWriteTxn<K, V> {
        /* Take the exclusive write lock first */
        let mguard = self.write.lock();
        /* Now take a ro-txn to get the data copied */
        let rguard = self.active.lock();
        /*
         * Take a ref to the root, we want to minimise our time in the.
         * active lock. We could do a full clone here but that would trigger
         * node-width worth of atomics, and if the write is dropped without
         * action we've save a lot of cycles.
         */
        let sblock: &SuperBlock<K, V> = rguard.as_ref();
        /* Setup the cursor that will work on the tree */
        let cursor = CursorWrite::new(sblock);

        /* Now build the write struct */
        BptreeMapWriteTxn {
            work: cursor,
            caller: self,
            _guard: mguard,
        }
        /* rguard dropped here */
    }

    /// Attempt to create a new write, returns None if another writer
    /// already exists.
    pub fn try_write(&self) -> Option<BptreeMapWriteTxn<K, V>> {
        self.write.try_lock().map(|mguard| {
            let rguard = self.active.lock();
            let sblock: &SuperBlock<K, V> = rguard.as_ref();
            let cursor = CursorWrite::new(sblock);
            BptreeMapWriteTxn {
                work: cursor,
                caller: self,
                _guard: mguard,
            }
        })
    }

    fn commit(&self, newdata: SuperBlock<K, V>) {
        // println!("commit wr");
        let mut rwguard = self.active.lock();
        // Now we need to setup the sb pointers properly.
        // The current active SHOULD have a NONE last seen as it's the current
        // tree holder.
        newdata.commit_prep(rwguard.as_ref());

        let arc_newdata = Arc::new(newdata);
        // Now pin the older to this new txn.
        {
            let mut pin_guard = rwguard.as_ref().pin_next.lock();
            *pin_guard = Some(arc_newdata.clone());
        }

        // Now push the new SB.
        *rwguard = arc_newdata;
    }
}

impl<K: Clone + Ord + Debug, V: Clone> FromIterator<(K, V)> for BptreeMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let temp_sb = SuperBlock::default();
        let mut cursor = CursorWrite::new(&temp_sb);
        cursor.extend(iter);

        let new_sblock = cursor.finalise();
        new_sblock.commit_prep(&temp_sb);

        BptreeMap {
            write: Mutex::new(()),
            active: Mutex::new(Arc::new(new_sblock)),
        }
    }
}

impl<'a, K: Clone + Ord + Debug, V: Clone> Extend<(K, V)> for BptreeMapWriteTxn<'a, K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        self.work.extend(iter);
    }
}

impl<'a, K: Clone + Ord + Debug, V: Clone> BptreeMapWriteTxn<'a, K, V> {
    // == RO methods

    /// Retrieve a value from the tree. If the value exists, a reference is returned
    /// as `Some(&V)`, otherwise if not present `None` is returned.
    pub fn get<'b, Q: ?Sized>(&'a self, k: &'b Q) -> Option<&'a V>
    where
        K: Borrow<Q>,
        Q: Ord,
    {
        self.work.search(k)
    }

    /// Assert if a key exists in the tree.
    pub fn contains_key<'b, Q: ?Sized>(&'a self, k: &'b Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord,
    {
        self.work.contains_key(k)
    }

    /// returns the current number of k:v pairs in the tree
    pub fn len(&self) -> usize {
        self.work.len()
    }

    /// Determine if the set is currently empty
    pub fn is_empty(&self) -> bool {
        self.work.len() == 0
    }

    // (adv) range

    /// Iterator over `(&K, &V)` of the set
    pub fn iter(&self) -> Iter<K, V> {
        self.work.kv_iter()
    }

    /// Iterator over &K
    pub fn values(&self) -> ValueIter<K, V> {
        self.work.v_iter()
    }

    /// Iterator over &V
    pub fn keys(&self) -> KeyIter<K, V> {
        self.work.k_iter()
    }

    // (adv) keys

    // (adv) values

    pub(crate) fn get_txid(&self) -> u64 {
        self.work.get_txid()
    }

    // == RW methods

    /// Reset this tree to an empty state. As this is within the transaction this
    /// change only takes effect once commited.
    pub fn clear(&mut self) {
        self.work.clear()
    }

    /// Insert or update a value by key. If the value previously existed it is returned
    /// as `Some(V)`. If the value did not previously exist this returns `None`.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.work.insert(k, v)
    }

    /// Remove a key if it exists in the tree. If the value exists, we return it as `Some(V)`,
    /// and if it did not exist, we return `None`
    pub fn remove(&mut self, k: &K) -> Option<V> {
        self.work.remove(k)
    }

    // split_off
    /*
    pub fn split_off_gte(&mut self, key: &K) -> BptreeMap<K, V> {
        unimplemented!();
    }
    */

    /// Remove all values less than (but not including) key from the map.
    pub fn split_off_lt(&mut self, key: &K) {
        self.work.split_off_lt(key)
    }

    // ADVANCED
    // append (join two sets)

    /// Get a mutable reference to a value in the tree. This is correctly, and
    /// safely cloned before you attempt to mutate the value, isolating it from
    /// other transactions.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.work.get_mut_ref(key)
    }

    // range_mut

    // entry

    // iter_mut

    /*
    /// Compact the tree structure if the density is below threshold, yielding improved search
    /// performance and lowering memory footprint.
    ///
    /// Many tree structures attempt to remain "balanced" consuming excess memory to allow
    /// amortizing cost and distributing values over the structure. Generally this means that
    /// a classic B+Tree has only ~55% to ~66% occupation of it's leaves (varying based on their
    /// width). The branches have a similar layout.
    ///
    /// Given linear (ordered) inserts this structure will have 100% utilisation at the leaves
    /// and between ~66% to ~75% occupation through out the branches. If you built this from a
    /// iterator, this is probably the case you have here!
    ///
    /// However under random insert loads we tend toward ~60% utilisation similar to the classic
    /// B+tree.
    ///
    /// Instead of paying a cost in time and memory on every insert to achieve the "constant" %60
    /// loading, we prefer to minimise the work in the tree in favour of compacting the structure
    /// when required. This is especially visible given that most workloads are linear or random
    /// and we save time on these workloads by not continually rebalancing.
    ///
    /// If you call this function, and the current occupation is less than 50% the tree will be
    /// rebalanced. This may briefly consume more ram, but will achieve a near ~100% occupation
    /// of k:v in the tree, with a reduction in leaves and branches.
    ///
    /// The net result is a short term stall, for long term lower memory usage and faster
    /// search response times.
    ///
    /// You should consider using this "randomly" IE 1 in X commits, so that you are not
    /// walking the tree continually, after a large randomise insert, or when memory
    /// pressure is high.
    pub fn compact(&mut self) -> bool {
        let (l, m) = self.work.tree_density();
        if l > 0 && (m / l) > 1 {
            self.compact_force();
            true
        } else {
            false
        }
    }

    /// Initiate a compaction of the tree regardless of it's density or loading factors.
    ///
    /// You probably should use `compact()` instead.
    ///
    /// See `compact()` for the logic of why this exists.
    pub fn compact_force(&mut self) {
        let mut par_cursor = CursorWrite::new(SuperBlock::default());
        par_cursor.extend(self.iter().map(|(kr, vr)| (kr.clone(), vr.clone())));

        // Now swap them over.
        // std::mem::swap(&mut self.work, &mut par_cursor);
        unimplemented!();
    }

    #[cfg(test)]
    pub(crate) fn tree_density(&self) -> (usize, usize) {
        self.work.tree_density()
    }
    */

    #[cfg(test)]
    pub(crate) fn verify(&self) -> bool {
        self.work.verify()
    }

    /// Create a read-snapshot of the current tree. This does NOT guarantee the tree may
    /// not be mutated during the read, so you MUST guarantee that no functions of the
    /// write txn are called while this snapshot is active.
    pub fn to_snapshot(&'a self) -> BptreeMapReadSnapshot<K, V> {
        BptreeMapReadSnapshot {
            work: SnapshotType::W(&self.work),
        }
    }

    /// Commit the changes from this write transaction. Readers after this point
    /// will be able to percieve these changes.
    ///
    /// To abort (unstage changes), just do not call this function.
    pub fn commit(self) {
        self.caller.commit(self.work.finalise())
    }
}

impl<'a, K: Clone + Ord + Debug, V: Clone> BptreeMapReadTxn<'a, K, V> {
    /// Retrieve a value from the tree. If the value exists, a reference is returned
    /// as `Some(&V)`, otherwise if not present `None` is returned.
    pub fn get<Q: ?Sized>(&'a self, k: &'a Q) -> Option<&'a V>
    where
        K: Borrow<Q>,
        Q: Ord,
    {
        self.work.search(k)
    }

    /// Assert if a key exists in the tree.
    pub fn contains_key<'b, Q: ?Sized>(&'a self, k: &'b Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord,
    {
        self.work.contains_key(k)
    }

    /// Returns the current number of k:v pairs in the tree
    pub fn len(&self) -> usize {
        self.work.len()
    }

    /// Determine if the set is currently empty
    pub fn is_empty(&self) -> bool {
        self.work.len() == 0
    }

    // (adv) range
    pub(crate) fn get_txid(&self) -> u64 {
        self.work.get_txid()
    }

    /// Iterator over `(&K, &V)` of the set
    pub fn iter(&self) -> Iter<K, V> {
        self.work.kv_iter()
    }

    /// Iterator over &K
    pub fn values(&self) -> ValueIter<K, V> {
        self.work.v_iter()
    }

    /// Iterator over &V
    pub fn keys(&self) -> KeyIter<K, V> {
        self.work.k_iter()
    }

    /// Create a read-snapshot of the current tree.
    /// As this is the read variant, it IS safe, and guaranteed the tree will not change.
    pub fn to_snapshot(&'a self) -> BptreeMapReadSnapshot<K, V> {
        BptreeMapReadSnapshot {
            work: SnapshotType::R(&self.work), // pin: PhantomData,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn verify(&self) -> bool {
        self.work.verify()
    }
}

impl<'a, K: Clone + Ord + Debug, V: Clone> BptreeMapReadSnapshot<'a, K, V> {
    /// Retrieve a value from the tree. If the value exists, a reference is returned
    /// as `Some(&V)`, otherwise if not present `None` is returned.
    pub fn get<Q: ?Sized>(&'a self, k: &'a Q) -> Option<&'a V>
    where
        K: Borrow<Q>,
        Q: Ord,
    {
        match self.work {
            SnapshotType::R(work) => work.search(k),
            SnapshotType::W(work) => work.search(k),
        }
    }

    /// Assert if a key exists in the tree.
    pub fn contains_key<'b, Q: ?Sized>(&'a self, k: &'b Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord,
    {
        match self.work {
            SnapshotType::R(work) => work.contains_key(k),
            SnapshotType::W(work) => work.contains_key(k),
        }
    }

    /// Returns the current number of k:v pairs in the tree
    pub fn len(&self) -> usize {
        match self.work {
            SnapshotType::R(work) => work.len(),
            SnapshotType::W(work) => work.len(),
        }
    }

    /// Determine if the set is currently empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // (adv) range

    /// Iterator over `(&K, &V)` of the set
    pub fn iter(&self) -> Iter<K, V> {
        match self.work {
            SnapshotType::R(work) => work.kv_iter(),
            SnapshotType::W(work) => work.kv_iter(),
        }
    }

    /// Iterator over &K
    pub fn values(&self) -> ValueIter<K, V> {
        match self.work {
            SnapshotType::R(work) => work.v_iter(),
            SnapshotType::W(work) => work.v_iter(),
        }
    }

    /// Iterator over &V
    pub fn keys(&self) -> KeyIter<K, V> {
        match self.work {
            SnapshotType::R(work) => work.k_iter(),
            SnapshotType::W(work) => work.k_iter(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::node::{assert_released, L_CAPACITY};
    use super::BptreeMap;
    // use rand::prelude::*;
    use rand::seq::SliceRandom;
    use std::iter::FromIterator;

    #[test]
    fn test_bptree2_map_basic_write() {
        let bptree: BptreeMap<usize, usize> = BptreeMap::new();
        {
            let mut bpwrite = bptree.write();
            // We should be able to insert.
            bpwrite.insert(0, 0);
            bpwrite.insert(1, 1);
            assert!(bpwrite.get(&0) == Some(&0));
            assert!(bpwrite.get(&1) == Some(&1));
            bpwrite.insert(2, 2);
            bpwrite.commit();
            // println!("commit");
        }
        {
            // Do a clear, but roll it back.
            let mut bpwrite = bptree.write();
            bpwrite.clear();
            // DO NOT commit, this triggers the rollback.
            // println!("post clear");
        }
        {
            let bpwrite = bptree.write();
            assert!(bpwrite.get(&0) == Some(&0));
            assert!(bpwrite.get(&1) == Some(&1));
            // println!("fin write");
        }
        std::mem::drop(bptree);
        assert_released();
    }

    #[test]
    fn test_bptree2_map_cursed_get_mut() {
        let bptree: BptreeMap<usize, usize> = BptreeMap::new();
        {
            let mut w = bptree.write();
            w.insert(0, 0);
            w.commit();
        }
        let r1 = bptree.read();
        {
            let mut w = bptree.write();
            let cursed_zone = w.get_mut(&0).unwrap();
            *cursed_zone = 1;
            // Correctly fails to work as it's a second borrow, which isn't
            // possible once w.remove occurs
            // w.remove(&0);
            // *cursed_zone = 2;
            w.commit();
        }
        let r2 = bptree.read();
        assert!(r1.get(&0) == Some(&0));
        assert!(r2.get(&0) == Some(&1));

        /*
        // Correctly fails to compile. PHEW!
        let fail = {
            let mut w = bptree.write();
            w.get_mut(&0).unwrap()
        };
        */
        std::mem::drop(r1);
        std::mem::drop(r2);
        std::mem::drop(bptree);
        assert_released();
    }

    #[test]
    fn test_bptree2_map_from_iter_1() {
        let ins: Vec<usize> = (0..(L_CAPACITY << 4)).collect();

        let map = BptreeMap::from_iter(ins.into_iter().map(|v| (v, v)));

        {
            let w = map.write();
            assert!(w.verify());
        }
        // assert!(w.tree_density() == ((L_CAPACITY << 4), (L_CAPACITY << 4)));
        std::mem::drop(map);
        assert_released();
    }

    #[test]
    fn test_bptree2_map_from_iter_2() {
        let mut rng = rand::thread_rng();
        let mut ins: Vec<usize> = (0..(L_CAPACITY << 4)).collect();
        ins.shuffle(&mut rng);

        let map = BptreeMap::from_iter(ins.into_iter().map(|v| (v, v)));

        {
            let w = map.write();
            assert!(w.verify());
            // w.compact_force();
            assert!(w.verify());
            // assert!(w.tree_density() == ((L_CAPACITY << 4), (L_CAPACITY << 4)));
        }

        std::mem::drop(map);
        assert_released();
    }

    fn bptree_map_basic_concurrency(lower: usize, upper: usize) {
        // Create a map
        let map = BptreeMap::new();

        // add values
        {
            let mut w = map.write();
            w.extend((0..lower).map(|v| (v, v)));
            w.commit();
        }

        // read
        let r = map.read();
        assert!(r.len() == lower);
        for i in 0..lower {
            assert!(r.contains_key(&i))
        }

        // Check a second write doesn't interfere
        {
            let mut w = map.write();
            w.extend((lower..upper).map(|v| (v, v)));
            w.commit();
        }

        assert!(r.len() == lower);

        // But a new write can see
        let r2 = map.read();
        assert!(r2.len() == upper);
        for i in 0..upper {
            assert!(r2.contains_key(&i))
        }

        // Now drain the tree, and the reader should be unaffected.
        {
            let mut w = map.write();
            for i in 0..upper {
                assert!(w.remove(&i).is_some())
            }
            w.commit();
        }

        // All consistent!
        assert!(r.len() == lower);
        assert!(r2.len() == upper);
        for i in 0..upper {
            assert!(r2.contains_key(&i))
        }

        let r3 = map.read();
        // println!("{:?}", r3.len());
        assert!(r3.len() == 0);

        std::mem::drop(r);
        std::mem::drop(r2);
        std::mem::drop(r3);

        std::mem::drop(map);
        assert_released();
    }

    #[test]
    fn test_bptree2_map_acb_order() {
        // Need to ensure that txns are dropped in order.

        // Add data, enouugh to cause a split. All data should be *2
        let map = BptreeMap::new();
        // add values
        {
            let mut w = map.write();
            w.extend((0..(L_CAPACITY * 2)).map(|v| (v * 2, v * 2)));
            w.commit();
        }
        let ro_txn_a = map.read();

        // New write, add 1 val
        {
            let mut w = map.write();
            w.insert(1, 1);
            w.commit();
        }

        let ro_txn_b = map.read();
        // ro_txn_b now owns nodes from a

        // New write, update a value
        {
            let mut w = map.write();
            w.insert(1, 10001);
            w.commit();
        }

        let ro_txn_c = map.read();
        // ro_txn_c
        // Drop ro_txn_b
        assert!(ro_txn_b.verify());
        std::mem::drop(ro_txn_b);
        // Are both still valid?
        assert!(ro_txn_a.verify());
        assert!(ro_txn_c.verify());
        // Drop remaining
        std::mem::drop(ro_txn_a);
        std::mem::drop(ro_txn_c);
        std::mem::drop(map);
        assert_released();
    }

    #[test]
    fn test_bptree2_map_weird_txn_behaviour() {
        let map: BptreeMap<usize, usize> = BptreeMap::new();

        let mut wr = map.write();
        let rd = map.read();

        wr.insert(1, 1);
        assert!(rd.get(&1) == None);
        wr.commit();
        assert!(rd.get(&1) == None);
    }

    fn test_bptree2_map_basic_concurrency_small() {
        bptree_map_basic_concurrency(100, 200)
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_bptree2_map_basic_concurrency_large() {
        bptree_map_basic_concurrency(10_000, 20_000)
    }

    /*
    #[test]
    fn test_bptree2_map_write_compact() {
        let mut rng = rand::thread_rng();
        let insa: Vec<usize> = (0..(L_CAPACITY << 4)).collect();

        let map = BptreeMap::from_iter(insa.into_iter().map(|v| (v, v)));

        let mut w = map.write();
        // Created linearly, should not need compact
        assert!(w.compact() == false);
        assert!(w.verify());
        assert!(w.tree_density() == ((L_CAPACITY << 4), (L_CAPACITY << 4)));

        // Even in reverse, we shouldn't need it ...
        let insb: Vec<usize> = (0..(L_CAPACITY << 4)).collect();
        let bmap = BptreeMap::from_iter(insb.into_iter().rev().map(|v| (v, v)));
        let mut bw = bmap.write();
        assert!(bw.compact() == false);
        assert!(bw.verify());
        // Assert the density is "best"
        assert!(bw.tree_density() == ((L_CAPACITY << 4), (L_CAPACITY << 4)));

        // Random however, may.
        let mut insc: Vec<usize> = (0..(L_CAPACITY << 4)).collect();
        insc.shuffle(&mut rng);
        let cmap = BptreeMap::from_iter(insc.into_iter().map(|v| (v, v)));
        let mut cw = cmap.write();
        let (_n, d1) = cw.tree_density();
        cw.compact_force();
        assert!(cw.verify());
        let (_n, d2) = cw.tree_density();
        assert!(d2 <= d1);
    }
    */

    /*
    use std::sync::atomic::{AtomicUsize, Ordering};
    use crossbeam_utils::thread::scope;
    use rand::Rng;
    const MAX_TARGET: usize = 210_000;

    #[test]
    fn test_bptree2_map_thread_stress() {
        let start = time::now();
        let reader_completions = AtomicUsize::new(0);
        // Setup a tree with some initial data.
        let map: BptreeMap<usize, usize> = BptreeMap::from_iter(
            (0..10_000).map(|v| (v, v))
        );
        // now setup the threads.
        scope(|scope| {
            let mref = &map;
            let rref = &reader_completions;

            let _readers: Vec<_> = (0..7)
                .map(|_| {
                    scope.spawn(move || {
                        println!("Started reader ...");
                        let mut rng = rand::thread_rng();
                        let mut proceed = true;
                        while proceed {
                            let m_read = mref.read();
                            proceed = ! m_read.contains_key(&MAX_TARGET);
                            // Get a random number.
                            // Add 10_000 * random
                            // Remove 10_000 * random
                            let v1 = rng.gen_range(1, 18) * 10_000;
                            let r1 = v1 + 10_000;
                            for i in v1..r1 {
                                m_read.get(&i);
                            }
                            assert!(m_read.verify());
                            rref.fetch_add(1, Ordering::Relaxed);
                        }
                        println!("Closing reader ...");
                    })
                })
                .collect();

            let _writers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(move || {
                        println!("Started writer ...");
                        let mut rng = rand::thread_rng();
                        let mut proceed = true;
                        while proceed {
                            let mut m_write = mref.write();
                            proceed = ! m_write.contains_key(&MAX_TARGET);
                            // Get a random number.
                            // Add 10_000 * random
                            // Remove 10_000 * random
                            let v1 = rng.gen_range(1, 18) * 10_000;
                            let r1 = v1 + 10_000;
                            let v2 = rng.gen_range(1, 19) * 10_000;
                            let r2 = v2 + 10_000;

                            for i in v1..r1 {
                                m_write.insert(i, i);
                            }
                            for i in v2..r2 {
                                m_write.remove(&i);
                            }
                            m_write.commit();
                        }
                        println!("Closing writer ...");
                    })
                })
                .collect();

            let _complete = scope.spawn(move || {
                let mut last_value = 200_000;
                while last_value < MAX_TARGET {
                    let mut m_write = mref.write();
                    last_value += 1;
                    if last_value % 1000 == 0 {
                        println!("{:?}", last_value);
                    }
                    m_write.insert(last_value, last_value);
                    assert!(m_write.verify());
                    m_write.commit();
                }
            });

        });
        let end = time::now();
        print!("BptreeMap MT create :{} reader completions :{}", end - start, reader_completions.load(Ordering::Relaxed));
        // Done!
    }

    #[test]
    fn test_std_mutex_btreemap_thread_stress() {
        use std::collections::BTreeMap;
        use std::sync::Mutex;

        let start = time::now();
        let reader_completions = AtomicUsize::new(0);
        // Setup a tree with some initial data.
        let map: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::from_iter(
            (0..10_000).map(|v| (v, v))
        ));
        // now setup the threads.
        scope(|scope| {
            let mref = &map;
            let rref = &reader_completions;

            let _readers: Vec<_> = (0..7)
                .map(|_| {
                    scope.spawn(move || {
                        println!("Started reader ...");
                        let mut rng = rand::thread_rng();
                        let mut proceed = true;
                        while proceed {
                            let m_read = mref.lock().unwrap();
                            proceed = ! m_read.contains_key(&MAX_TARGET);
                            // Get a random number.
                            // Add 10_000 * random
                            // Remove 10_000 * random
                            let v1 = rng.gen_range(1, 18) * 10_000;
                            let r1 = v1 + 10_000;
                            for i in v1..r1 {
                                m_read.get(&i);
                            }
                            rref.fetch_add(1, Ordering::Relaxed);
                        }
                        println!("Closing reader ...");
                    })
                })
                .collect();

            let _writers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(move || {
                        println!("Started writer ...");
                        let mut rng = rand::thread_rng();
                        let mut proceed = true;
                        while proceed {
                            let mut m_write = mref.lock().unwrap();
                            proceed = ! m_write.contains_key(&MAX_TARGET);
                            // Get a random number.
                            // Add 10_000 * random
                            // Remove 10_000 * random
                            let v1 = rng.gen_range(1, 18) * 10_000;
                            let r1 = v1 + 10_000;
                            let v2 = rng.gen_range(1, 19) * 10_000;
                            let r2 = v2 + 10_000;

                            for i in v1..r1 {
                                m_write.insert(i, i);
                            }
                            for i in v2..r2 {
                                m_write.remove(&i);
                            }
                        }
                        println!("Closing writer ...");
                    })
                })
                .collect();

            let _complete = scope.spawn(move || {
                let mut last_value = 200_000;
                while last_value < MAX_TARGET {
                    let mut m_write = mref.lock().unwrap();
                    last_value += 1;
                    if last_value % 1000 == 0 {
                        println!("{:?}", last_value);
                    }
                    m_write.insert(last_value, last_value);
                }
            });

        });
        let end = time::now();
        print!("Mutex<BTreeMap> MT create :{} reader completions :{}", end - start, reader_completions.load(Ordering::Relaxed));
        // Done!
    }

    #[test]
    fn test_std_rwlock_btreemap_thread_stress() {
        use std::collections::BTreeMap;
        use std::sync::RwLock;

        let start = time::now();
        let reader_completions = AtomicUsize::new(0);
        // Setup a tree with some initial data.
        let map: RwLock<BTreeMap<usize, usize>> = RwLock::new(BTreeMap::from_iter(
            (0..10_000).map(|v| (v, v))
        ));
        // now setup the threads.
        scope(|scope| {
            let mref = &map;
            let rref = &reader_completions;

            let _readers: Vec<_> = (0..7)
                .map(|_| {
                    scope.spawn(move || {
                        println!("Started reader ...");
                        let mut rng = rand::thread_rng();
                        let mut proceed = true;
                        while proceed {
                            let m_read = mref.read().unwrap();
                            proceed = ! m_read.contains_key(&MAX_TARGET);
                            // Get a random number.
                            // Add 10_000 * random
                            // Remove 10_000 * random
                            let v1 = rng.gen_range(1, 18) * 10_000;
                            let r1 = v1 + 10_000;
                            for i in v1..r1 {
                                m_read.get(&i);
                            }
                            rref.fetch_add(1, Ordering::Relaxed);
                        }
                        println!("Closing reader ...");
                    })
                })
                .collect();

            let _writers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(move || {
                        println!("Started writer ...");
                        let mut rng = rand::thread_rng();
                        let mut proceed = true;
                        while proceed {
                            let mut m_write = mref.write().unwrap();
                            proceed = ! m_write.contains_key(&MAX_TARGET);
                            // Get a random number.
                            // Add 10_000 * random
                            // Remove 10_000 * random
                            let v1 = rng.gen_range(1, 18) * 10_000;
                            let r1 = v1 + 10_000;
                            let v2 = rng.gen_range(1, 19) * 10_000;
                            let r2 = v2 + 10_000;

                            for i in v1..r1 {
                                m_write.insert(i, i);
                            }
                            for i in v2..r2 {
                                m_write.remove(&i);
                            }
                        }
                        println!("Closing writer ...");
                    })
                })
                .collect();

            let _complete = scope.spawn(move || {
                let mut last_value = 200_000;
                while last_value < MAX_TARGET {
                    let mut m_write = mref.write().unwrap();
                    last_value += 1;
                    if last_value % 1000 == 0 {
                        println!("{:?}", last_value);
                    }
                    m_write.insert(last_value, last_value);
                }
            });

        });
        let end = time::now();
        print!("RwLock<BTreeMap> MT create :{} reader completions :{}", end - start, reader_completions.load(Ordering::Relaxed));
        // Done!
    }
    */
}

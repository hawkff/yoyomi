//! The kitty images this process keeps in the terminal.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use parking_lot::Mutex;
use yoyomi_core::{ImageId, ImageIds};

/// A claim on a kitty image id. It holds until the registry gives the id to
/// someone else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lease {
    pub id: ImageId,
    generation: u64,
}

/// Which kitty image ids are in use, least recently shown first. When every
/// id is taken, the least recently shown one goes to the next caller. Reusing
/// an id needs no delete, because every kitty upload deletes its id first.
/// The ids that frames on screen draw are pinned: no lease takes them, and
/// once given back they leave through `take_freed` only after the frames
/// that replace them show.
pub struct Resident {
    ids: ImageIds,
    resend: bool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    leases: HashMap<ImageId, u64>,
    order: VecDeque<ImageId>,
    generation: u64,
    pinned: HashSet<ImageId>,
    /// Ids whose lease ended, until `take_freed` returns them or a lease
    /// takes them again.
    freed: BTreeSet<ImageId>,
}

impl Resident {
    /// A registry over `ids`. With `resend`, as under tmux, an upload goes out
    /// only with a frame that draws its image, and again whenever a frame
    /// draws the image anew, because tmux drops passthrough that arrives while
    /// it redraws.
    pub fn new(ids: ImageIds, resend: bool) -> Self {
        Resident { ids, resend, state: Mutex::new(State::default()) }
    }

    /// Whether uploads go out with the frames that draw their images.
    pub fn resend(&self) -> bool {
        self.resend
    }

    /// Claims an id that is not pinned: a free one, or else the least
    /// recently shown.
    pub fn lease(&self) -> Lease {
        let mut state = self.state.lock();
        let id = match self.take_unpinned(&state.pinned) {
            Some(id) => id,
            None => match state.order.iter().position(|id| !state.pinned.contains(id)) {
                Some(oldest) => state.order.remove(oldest).expect("the position is in the order"),
                // Every id is pinned or leased and pinned: the pins give way,
                // and the image of the id taken may change on screen before
                // its frame is replaced. Frames draw a handful of the 255 ids.
                None => match self.ids.take() {
                    Some(id) => id,
                    None => state.order.pop_front().expect("every id the pool lacks has a lease"),
                },
            },
        };
        state.generation += 1;
        let generation = state.generation;
        state.leases.insert(id, generation);
        state.order.retain(|&held| held != id);
        state.order.push_back(id);
        Lease { id, generation }
    }

    /// A free id that is not pinned. The pinned ones it meets go back to the
    /// pool.
    fn take_unpinned(&self, pinned: &HashSet<ImageId>) -> Option<ImageId> {
        let mut skipped = Vec::new();
        let id = loop {
            match self.ids.take() {
                Some(id) if pinned.contains(&id) => skipped.push(id),
                id => break id,
            }
        };
        for id in skipped {
            self.ids.give_back(id);
        }
        id
    }

    /// Pins `ids`, the ones frames on screen draw, in place of those pinned
    /// before.
    pub fn pin(&self, ids: impl IntoIterator<Item = ImageId>) {
        self.state.lock().pinned = ids.into_iter().collect();
    }

    /// The ids given back since the last call that are neither pinned nor
    /// leased again: no frame on screen draws their images and nobody holds
    /// them. Each comes once; pinned ones wait for a later call, and ones
    /// leased again are dropped, because their next upload replaces the
    /// image.
    pub fn take_freed(&self) -> Vec<ImageId> {
        let mut state = self.state.lock();
        let State { leases, pinned, freed, .. } = &mut *state;
        let mut taken = Vec::new();
        freed.retain(|id| match (leases.contains_key(id), pinned.contains(id)) {
            (true, _) => false,
            (false, pinned) => {
                if !pinned {
                    taken.push(*id);
                }
                pinned
            }
        });
        taken
    }

    /// Whether `lease` still holds its id.
    pub fn holds(&self, lease: Lease) -> bool {
        self.state.lock().leases.get(&lease.id) == Some(&lease.generation)
    }

    /// Whether a lease holds `id`.
    pub fn leased(&self, id: ImageId) -> bool {
        self.state.lock().leases.contains_key(&id)
    }

    /// Marks the image of `lease` as shown now, so it goes last.
    pub fn touch(&self, lease: Lease) {
        let mut state = self.state.lock();
        if state.leases.get(&lease.id) == Some(&lease.generation) {
            state.order.retain(|&held| held != lease.id);
            state.order.push_back(lease.id);
        }
    }

    /// Gives the id of `lease` back to the pool.
    pub fn release(&self, lease: Lease) {
        let mut state = self.state.lock();
        if state.leases.get(&lease.id) == Some(&lease.generation) {
            state.leases.remove(&lease.id);
            state.order.retain(|&held| held != lease.id);
            state.freed.insert(lease.id);
            self.ids.give_back(lease.id);
        }
    }

    /// Every id this process may use, for freeing them all at exit.
    pub fn ids(&self) -> impl Iterator<Item = ImageId> {
        self.ids.all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_recently_shown_image_goes_first() {
        let resident = Resident::new(ImageIds::with_base(0x100), false);
        let leases: Vec<Lease> = (0..ImageIds::PER_PROCESS).map(|_| resident.lease()).collect();
        resident.touch(leases[0]);
        let next = resident.lease();
        assert_eq!(next.id, leases[1].id, "the second lease was the least recently shown");
        assert!(!resident.holds(leases[1]));
        assert!(resident.holds(leases[0]) && resident.holds(next));
    }

    #[test]
    fn released_ids_return_to_the_pool() {
        let resident = Resident::new(ImageIds::with_base(0x100), true);
        let lease = resident.lease();
        resident.release(lease);
        assert!(!resident.holds(lease));
        let again = resident.lease();
        assert_eq!(again.id, lease.id);
        assert_ne!(again, lease, "a new lease on the same id is a different claim");
        assert!(resident.resend());
    }

    #[test]
    fn a_pinned_id_is_neither_handed_out_nor_taken_from_its_lease() {
        let resident = Resident::new(ImageIds::with_base(0x100), false);
        let (shown, kept) = (resident.lease(), resident.lease());
        resident.pin([shown.id, kept.id]);
        resident.release(shown);
        let next = resident.lease();
        assert!(next.id != shown.id && next.id != kept.id, "{next:?} is pinned");
        // With the pool empty, the least recently shown lease that is not
        // pinned goes: not `kept`, the oldest.
        let rest: Vec<Lease> = (0..ImageIds::PER_PROCESS - 3).map(|_| resident.lease()).collect();
        let taken = resident.lease();
        assert_eq!(taken.id, next.id, "the oldest lease not pinned");
        assert!(resident.holds(kept) && !resident.holds(next) && rest.iter().all(|&lease| resident.holds(lease)));
        // Once unpinned, the id given back is free again.
        resident.pin([]);
        assert_eq!(resident.lease().id, shown.id);
    }

    #[test]
    fn freed_ids_wait_for_their_pins_and_come_once() {
        let resident = Resident::new(ImageIds::with_base(0x100), false);
        let (shown, reused, gone) = (resident.lease(), resident.lease(), resident.lease());
        resident.pin([shown.id]);
        for lease in [shown, reused, gone] {
            resident.release(lease);
        }
        // The lowest free id that is not pinned goes to a new lease, whose
        // next upload replaces the image.
        let renewed = resident.lease();
        assert_eq!(renewed.id, reused.id);
        assert_eq!(resident.take_freed(), [gone.id], "the pinned id waits and the leased one is dropped");
        assert!(resident.take_freed().is_empty(), "each id comes once");
        resident.pin([]);
        assert_eq!(resident.take_freed(), [shown.id]);
        assert!(resident.take_freed().is_empty());
        resident.release(renewed);
        assert_eq!(resident.take_freed(), [renewed.id]);
    }

    #[test]
    fn the_least_recently_shown_rule_holds_among_unpinned_ids() {
        let resident = Resident::new(ImageIds::with_base(0x100), false);
        let leases: Vec<Lease> = (0..ImageIds::PER_PROCESS).map(|_| resident.lease()).collect();
        resident.pin([leases[0].id, leases[1].id]);
        resident.touch(leases[2]);
        let next = resident.lease();
        assert_eq!(next.id, leases[3].id, "0 and 1 are pinned and 2 was shown last");
        assert!(resident.take_freed().is_empty(), "an id taken from its lease goes straight to the new one");
    }

    #[test]
    fn with_every_id_pinned_a_lease_still_comes() {
        let resident = Resident::new(ImageIds::with_base(0x100), false);
        let leases: Vec<Lease> = (0..ImageIds::PER_PROCESS).map(|_| resident.lease()).collect();
        resident.pin(resident.ids());
        let taken = resident.lease();
        assert_eq!(taken.id, leases[0].id, "the least recently shown");
        // Some ids given back and every one pinned: the pool hands one out.
        resident.release(leases[1]);
        assert_eq!(resident.lease().id, leases[1].id);
    }
}

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::network::protocol::MediaManifest;

const MAX_OFFERS: usize = 10;
const OFFER_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
pub struct OutgoingMedia {
    pub path: PathBuf,
    pub manifest: MediaManifest,
    pub allowed_peers: HashSet<String>,
    pub created_at: Instant,
}

#[derive(Debug)]
pub enum OfferLookup {
    Found(OutgoingMedia),
    NotFound,
    /// The hash is offered, but not to this peer.
    Denied,
}

/// Media we have offered and are willing to serve chunks for.
#[derive(Debug, Default)]
pub struct OutgoingMediaRegistry {
    offers: RwLock<VecDeque<OutgoingMedia>>,
}

impl OutgoingMediaRegistry {
    pub async fn register(&self, path: PathBuf, manifest: MediaManifest, peers: HashSet<String>) {
        let mut offers = self.offers.write().await;
        offers
            .retain(|o| o.manifest.sha256 != manifest.sha256 && o.created_at.elapsed() < OFFER_TTL);
        offers.push_back(OutgoingMedia {
            path,
            manifest,
            allowed_peers: peers,
            created_at: Instant::now(),
        });
        while offers.len() > MAX_OFFERS {
            offers.pop_front();
        }
    }

    pub async fn lookup(&self, sha256: &str, peer_id: &str) -> OfferLookup {
        let offers = self.offers.read().await;
        match offers
            .iter()
            .find(|o| o.manifest.sha256 == sha256 && o.created_at.elapsed() < OFFER_TTL)
        {
            Some(offer) if offer.allowed_peers.contains(peer_id) => {
                OfferLookup::Found(offer.clone())
            }
            Some(_) => OfferLookup::Denied,
            None => OfferLookup::NotFound,
        }
    }

    /// Stop serving a peer (called on unpair).
    pub async fn revoke_peer(&self, peer_id: &str) {
        let mut offers = self.offers.write().await;
        for offer in offers.iter_mut() {
            offer.allowed_peers.remove(peer_id);
        }
    }
}

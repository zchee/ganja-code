//! Independent public-API openers share only completed generations. The storage
//! crate's phase-barrier tests separately pin stale-probe revalidation under EX;
//! this integration witness races real starts without uncooperative renames.

use std::sync::Barrier;
use std::thread;

use ganja_core::{SessionId, Storage};
use ganja_testkit::{
    PRE_UUID_ID, plant_preuuid_store, seeded_session_info, set_aside_of, temp_dir,
};
use rusqlite::Connection;

const FIRST: &str = "0198f2c4-a1b0-7000-8000-000000000001";
const SECOND: &str = "0198f2c4-a1b0-7000-8000-000000000002";

#[test]
fn independent_openers_keep_the_winners_replacement_generation() {
    let directory = temp_dir();
    let root = directory.path().join("storage");
    let (planted, database) = plant_preuuid_store(root.clone());
    drop(planted);
    let barrier = Barrier::new(3);
    thread::scope(|scope| {
        let workers: Vec<_> = [FIRST, SECOND]
            .into_iter()
            .map(|id| {
                let root = &root;
                let barrier = &barrier;
                scope.spawn(move || {
                    let store = Storage::open(root.clone());
                    barrier.wait();
                    store
                        .save_info(&seeded_session_info(SessionId::from(id.to_owned()), 1))
                        .expect("racing public opener persists");
                    store
                })
            })
            .collect();
        barrier.wait();
        let stores: Vec<_> =
            workers.into_iter().map(|worker| worker.join().expect("opener joins")).collect();
        for store in &stores {
            let mut ids: Vec<_> = store
                .list_sessions()
                .expect("completed generation reads")
                .into_iter()
                .map(|info| info.id.as_str().to_owned())
                .collect();
            ids.sort();
            assert_eq!(
                ids,
                [FIRST, SECOND],
                "neither opener may move the replacement out from under the other"
            );
        }
    });
    let kept = set_aside_of(directory.path(), &database);
    assert_eq!(kept.len(), 1, "only the old generation is archived");
    let archived =
        Connection::open(directory.path().join(&kept[0])).expect("ordinary archived-set read");
    let ids: Vec<String> = archived
        .prepare("SELECT id FROM session ORDER BY id")
        .expect("archived schema")
        .query_map([], |row| row.get(0))
        .expect("archived ids")
        .collect::<Result<_, _>>()
        .expect("all archived rows");
    assert_eq!(ids, [PRE_UUID_ID], "the retained generation is the original, not a replacement");
}

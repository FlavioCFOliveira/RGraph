use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rgraph::index::btree::{BPlusTree, BPlusTreeConfig};

fn btree_insert(c: &mut Criterion) {
    let config = BPlusTreeConfig::default();
    let tree = BPlusTree::new(config);

    c.bench_function("btree_insert_1k", |b| {
        let mut key = 0u64;
        b.iter(|| {
            let k = key.to_be_bytes().to_vec();
            let v = vec![0u8; 32];
            let _ = tree.insert(&k, &v);
            key += 1;
        });
    });
}

fn btree_search(c: &mut Criterion) {
    let config = BPlusTreeConfig::default();
    let tree = BPlusTree::new(config);
    for i in 0..1000 {
        let k = i.to_be_bytes().to_vec();
        let v = vec![0u8; 32];
        let _ = tree.insert(&k, &v);
    }

    c.bench_function("btree_search_1k", |b| {
        let mut key = 0u64;
        b.iter(|| {
            let k = key.to_be_bytes().to_vec();
            let _ = tree.search(&k);
            key = (key + 1) % 1000;
        });
    });
}

criterion_group!(benches, btree_insert, btree_search);
criterion_main!(benches);

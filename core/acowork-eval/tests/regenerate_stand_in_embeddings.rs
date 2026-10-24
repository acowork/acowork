// Regenerates `dev/eval/retrieval/embeddings.json` from the kernel's own
// stand-in projection, and checks that the checked-in file is current.
//
// The file is checked in, but it is derived data: it is `tokenize` + `project`
// applied to `corpus.jsonl`. Committing the output instead of generating it at
// read time is what lets a real embedding model be dropped in later, so the
// trade-off is that it can go stale — `EvalDataset::load` refuses to read a
// stale file rather than letting the two harnesses measure different things.
// This test is the repair path that panic points at.
//
// Ignored because it writes into the working tree; run it deliberately after
// changing the tokenizer, the stand-in dimension, or the corpus text. The
// freshness half of this file is NOT ignored, so an ordinary test run is what
// enforces the invariant:
//
//   cargo test -p acowork-eval --test regenerate_stand_in_embeddings -- --ignored
//
// Generator and validator are the same code, so a run that reports success both
// rewrites the file and proves the reader accepts it.

use acowork_eval::{EvalDataset, STAND_IN_DIM, project, stand_in_embeddings_file};

#[test]
#[ignore = "writes into the working tree — run deliberately after changing the tokenizer or corpus"]
fn regenerate_stand_in_embeddings() {
    let dir = EvalDataset::dir();
    let corpus = EvalDataset::corpus_rows(&dir);
    assert!(
        !corpus.is_empty(),
        "no corpus at {} — refusing to write an empty embeddings file over the dataset",
        dir.display()
    );

    let path = dir.join("embeddings.json");
    std::fs::write(&path, format!("{}\n", stand_in_embeddings_file(&corpus)))
        .expect("write embeddings.json");

    // Read it back through the same validator every harness uses. If the
    // generator and the checker ever disagree, this is where it surfaces.
    let ds = EvalDataset::load_repo();
    assert_eq!(
        ds.dim, STAND_IN_DIM,
        "the regenerated file declares dim {}, not the kernel's {STAND_IN_DIM}",
        ds.dim
    );
    assert_eq!(
        ds.vectors.len(),
        corpus.len(),
        "the regenerated file covers {} rows of {}",
        ds.vectors.len(),
        corpus.len()
    );
    for (i, row) in corpus.iter().enumerate() {
        assert_eq!(
            ds.vector(i),
            project(&row.content, STAND_IN_DIM),
            "row {i} does not round-trip through the written file",
        );
    }
}

/// The invariant as a default-running test, so it is not gated behind an ignored
/// eval. Leaving `embeddings.json` behind a tokenizer change would otherwise stay
/// invisible until someone happens to run a `--ignored` harness, and the first
/// thing that harness reports would be a semantic-leg number computed across two
/// different tokenizers.
#[test]
fn checked_in_vectors_match_the_kernel() {
    let ds = EvalDataset::load_repo();
    assert_eq!(
        ds.corpus.len(),
        ds.vectors.len(),
        "embeddings.json covers {} rows of {}",
        ds.vectors.len(),
        ds.corpus.len()
    );
}

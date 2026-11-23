#[test]
#[ignore]
fn probe() {
    let p = std::env::var("PROBE_REPO").unwrap();
    let repo = git2::Repository::open(&p).unwrap();
    let r = gityas_git::branches::checkout_branch(&repo, &std::env::var("PROBE_REF").unwrap());
    eprintln!("result: {r:?}");
}

fn main() {
    let cfg = appcipe_spec::from_file("examples/appcipe.yml").expect("parse failed");
    println!("{:#?}", cfg);
}

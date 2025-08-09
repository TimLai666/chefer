fn main() {
    let cfg = appcipe_spec::parse_appcipe_from_file("examples/appcipe.yml").expect("parse failed");
    println!("{:#?}", cfg);
}

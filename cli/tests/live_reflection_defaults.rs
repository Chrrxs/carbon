use rbx_dom_weak::{
	types::{Variant, Vector3},
	InstanceBuilder, Ustr, WeakDom,
};

const LIVE_API_DUMP_WITHOUT_DEFAULTS: &str = r#"{
	"Version": 1,
	"Classes": [{
		"Name": "Instance",
		"Superclass": "<<<ROOT>>>",
		"Tags": [],
		"Members": []
	}, {
		"Name": "PVInstance",
		"Superclass": "Instance",
		"Tags": [],
		"Members": []
	}, {
		"Name": "BasePart",
		"Superclass": "PVInstance",
		"Tags": [],
		"Members": [{
			"MemberType": "Property",
			"Name": "Size",
			"ValueType": { "Category": "DataType", "Name": "Vector3" },
			"Security": { "Read": "None", "Write": "None" },
			"Tags": [],
			"Serialization": { "CanLoad": true, "CanSave": true }
		}]
	}, {
		"Name": "Part",
		"Superclass": "BasePart",
		"Tags": [],
		"Members": []
	}],
	"Enums": []
}"#;

#[test]
fn live_reflection_preserves_part_size_default() {
	let reflection = carbon::util::init_reflection_from_json(LIVE_API_DUMP_WITHOUT_DEFAULTS, [0, 732, 0, 1])
		.expect("initialize live reflection");
	let dom = WeakDom::new(
		InstanceBuilder::new("DataModel")
			.with_child(
				InstanceBuilder::new("Part")
					.with_name("ExplicitSize")
					.with_property("Size", Vector3::new(8.0, 8.0, 8.0)),
			)
			.with_child(InstanceBuilder::new("Part").with_name("DefaultSize")),
	);
	let roots = dom.root().children().to_vec();

	let mut encoded = Vec::new();
	rbx_binary::to_writer_with_database(&mut encoded, &dom, &roots, &reflection.database)
		.expect("serialize Parts using live reflection");
	let decoded = rbx_binary::from_reader_with_database(encoded.as_slice(), &reflection.database)
		.expect("deserialize Parts using live reflection");
	let default_part = decoded
		.root()
		.children()
		.iter()
		.map(|referent| decoded.get_by_ref(*referent).expect("decoded Part"))
		.find(|instance| instance.name == "DefaultSize")
		.expect("default-sized Part");

	assert_eq!(
		default_part.properties.get(&Ustr::from("Size")),
		Some(&Variant::Vector3(Vector3::new(4.0, 1.2, 2.0)))
	);
}

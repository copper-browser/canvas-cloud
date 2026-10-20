use super::*;

fn rows(labels: &[(&str, &str)]) -> Vec<Row> {
    labels
        .iter()
        .map(|(id, label)| Row {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
        })
        .collect()
}

#[test]
fn reads_tolerantly() {
    assert_eq!(read_columns(None), ["Yes", "No"]);
    assert_eq!(read_note_columns(None), ["Done"]);
    assert_eq!(
        read_columns(Some(&json!([
            "Yes", "yes", "", 3, "No", "Maybe", "Later", "Never"
        ]))),
        ["Yes", "No", "Maybe", "Later"]
    );
    assert_eq!(
        read_rows(Some(
            &json!([{"id": "a", "label": "A"}, {"id": "a", "label": "dup"}, {"label": "no id"}, "str", {"id": "b"}])
        )),
        rows(&[("a", "A"), ("b", "")])
    );
    assert_eq!(read_pick(Some(&json!({"col": ""}))), None);
    assert_eq!(
        read_pick(Some(&json!({"col": "Yes", "at": 5}))),
        Some(Pick {
            col: "Yes".into(),
            by: String::new(),
            by_id: String::new(),
            at: 5.0
        })
    );
    let people = read_assignee_list(Some(&json!([
        "Ann",
        {"name": "ann"},
        {"name": " @Bob ", "id": " U1 ", "email": "bob@x.io", "at": 0},
        {"name": ""},
        3
    ])));
    assert_eq!(people.len(), 2);
    assert_eq!(people[1].name, "Bob");
    assert_eq!(people[1].id.as_deref(), Some("u1"));
    assert_eq!(people[1].at, None);
}

#[test]
fn reads_a_note_checklist() {
    let text = "### Party\n- [x] Ann · No\n  > Late\n  @Grace\n- [ ] Bob";
    let get = |k: &str| -> Option<Value> {
        match k {
            "columns" => Some(json!(["Yes", "No"])),
            "who:ann" => Some(json!({"col": "no", "by": "Cy", "byId": "c", "at": 9})),
            "assignees:ann" => Some(json!([{"name": "grace", "id": "g1"}])),
            _ => None,
        }
    };
    let (title, c) = read_note_checklist(&get, text);
    assert_eq!(title, "Party");
    assert_eq!(c.rows, rows(&[("t0", "Ann"), ("t1", "Bob")]));
    let p = c.pick("t0").unwrap();
    assert_eq!((p.col.as_str(), p.by.as_str()), ("No", "Cy"));
    assert_eq!(c.note("t0").unwrap().text, "Late");
    assert_eq!(c.people("t0")[0].name, "Grace");
    assert_eq!(c.people("t0")[0].id.as_deref(), Some("g1"));
    assert_eq!(tally(&c), [("Yes".into(), 0), ("No".into(), 1)]);
    let s = note_row_summaries(&c, &|t| t.to_owned());
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(
        v,
        json!([
            {"label": "Ann", "pick": "No", "by": "Cy", "at": 9, "note": "Late",
             "assignees": [{"name": "Grace", "id": "g1"}]},
            {"label": "Bob", "pick": null}
        ])
    );
}

#[test]
fn cleans_columns() {
    assert_eq!(
        clean_columns(&json!([" Yes ", "No"])).unwrap(),
        ["Yes", "No"]
    );
    assert_eq!(
        clean_columns(&json!(["x".repeat(60)])).unwrap()[0].len(),
        40
    );
    assert!(clean_columns(&json!([])).unwrap_err().contains("1 to 4"));
    assert!(clean_columns(&json!(["a", "b", "c", "d", "e"]))
        .unwrap_err()
        .contains("1 to 4"));
    assert_eq!(
        clean_columns(&json!(["Yes", "yes"])).unwrap_err(),
        "`columns` has \"yes\" twice"
    );
    assert!(clean_columns(&json!(["Yes", ""]))
        .unwrap_err()
        .contains("non-empty"));
}

#[test]
fn cleans_rows_keeping_ids() {
    let existing = rows(&[("a", "Ann"), ("b", "Bob")]);
    let out = clean_rows(
        &json!(["Bob", {"label": "Ann", "id": "a"}, "Cy", {"text": "Dee"}]),
        &existing,
    )
    .unwrap();
    let labels: Vec<&str> = out.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["Bob", "Ann", "Cy", "Dee"]);
    assert_eq!(out[0].id, "b");
    assert_eq!(out[1].id, "a");
    assert!(out.iter().all(|r| valid_row_id(&r.id)));
    let mut ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 4);
    assert!(clean_rows(&json!("Ann"), &[]).unwrap_err().contains("list"));
    let many: Vec<String> = (0..61).map(|i| format!("p{i}")).collect();
    assert!(clean_rows(&json!(many), &[]).unwrap_err().contains("60"));
    assert!(clean_rows(&json!([{"label": "x", "id": "bad id!"}]), &[])
        .unwrap_err()
        .contains("id"));
    assert!(clean_rows(
        &json!([{"label": "x", "id": "a"}, {"label": "y", "id": "a"}]),
        &[]
    )
    .unwrap_err()
    .contains("twice"));
    assert!(clean_rows(&json!([{"label": 3}]), &[])
        .unwrap_err()
        .contains("string"));
}

#[test]
fn cleans_picks() {
    let rs = rows(&[("a", "Ann"), ("b", "Bob"), ("c", "bob")]);
    let cols = vec!["Yes".to_owned(), "No".to_owned()];
    let mut got = clean_picks(&json!({"a": "yes", "Bob": "No", "c": null}), &rs, &cols).unwrap();
    got.sort();
    assert_eq!(
        got,
        [
            ("a".to_owned(), Some("Yes".to_owned())),
            ("b".to_owned(), Some("No".to_owned())),
            ("c".to_owned(), None)
        ]
    );
    assert_eq!(
        clean_picks(&json!({"ann": true}), &rs, &["Done".to_owned()]).unwrap(),
        [("a".to_owned(), Some("Done".to_owned()))]
    );
    assert!(clean_picks(&json!({"BOB": "Yes"}), &rs, &cols)
        .unwrap_err()
        .contains("more than one"));
    assert_eq!(
        clean_picks(&json!({"Zed": "Yes"}), &rs, &cols).unwrap_err(),
        "`picks`: no row \"Zed\""
    );
    assert_eq!(
        clean_picks(&json!({"Ann": "Maybe"}), &rs, &cols).unwrap_err(),
        "`picks`: no column \"Maybe\" (Yes, No)"
    );
    assert!(clean_picks(&json!(["Ann"]), &rs, &cols)
        .unwrap_err()
        .contains("object"));
    assert!(clean_picks(&json!({"Ann": 1}), &rs, &cols)
        .unwrap_err()
        .contains("values"));
}

#[test]
fn cleans_notes_and_assignees() {
    let rs = rows(&[("t0", "Ann"), ("t1", "Bob")]);
    assert_eq!(
        clean_notes(&json!({"Ann": "a\r\nb", "t1": null}), &rs).unwrap(),
        [
            ("t0".to_owned(), "a\nb".to_owned()),
            ("t1".to_owned(), String::new())
        ]
    );
    assert_eq!(
        clean_notes(&json!({"Cy": "x"}), &rs).unwrap_err(),
        "`notes`: no row \"Cy\""
    );
    let people = vec![
        Person {
            id: "U-1".into(),
            name: "Grace Hopper".into(),
            email: Some("grace@x.io".into()),
        },
        Person {
            id: "u-2".into(),
            name: "Ada Lovelace".into(),
            email: None,
        },
    ];
    let got = clean_assignees(
        &json!({"Ann": ["gr", "grace@x.io", "@ada lovelace", "Zed"], "Bob": "new@y.io"}),
        &rs,
        &people,
    )
    .unwrap();
    let ann = &got[0].1;
    assert_eq!(ann.len(), 3, "{ann:?}");
    assert_eq!(ann[0].name, "Grace Hopper");
    assert_eq!(ann[0].id.as_deref(), Some("u-1"));
    assert_eq!(ann[1].name, "Ada Lovelace");
    assert_eq!(ann[2].name, "Zed");
    let bob = &got[1].1;
    assert_eq!(bob[0].email.as_deref(), Some("new@y.io"));
    let thirteen: Vec<String> = (0..13).map(|i| format!("p{i}")).collect();
    assert_eq!(
        clean_assignees(&json!({ "Ann": thirteen }), &rs, &people).unwrap_err(),
        "`assignees`: at most 12 people on a row"
    );
    assert!(clean_assignees(&json!({"Ann": [3]}), &rs, &people)
        .unwrap_err()
        .contains("names must be strings"));
}

#[test]
fn merges_assignees() {
    let by = Picker {
        name: "Ann",
        id: "a",
        at: 5,
    };
    let was = vec![Assignee {
        name: "Grace".into(),
        by: Some("Old".into()),
        by_id: Some("o".into()),
        at: Some(1.0),
        ..Assignee::default()
    }];
    let next = vec![
        Assignee {
            name: "grace".into(),
            id: Some("g".into()),
            ..Assignee::default()
        },
        Assignee {
            name: "Bob".into(),
            ..Assignee::default()
        },
    ];
    let (people, added) = merge_assignees(&was, &next, &by);
    assert_eq!(people[0].by.as_deref(), Some("Old"));
    assert_eq!(people[0].id.as_deref(), Some("g"));
    assert_eq!(people[0].name, "grace");
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].at, Some(5.0));
    assert_eq!(
        people[1].stored(),
        json!({"name": "Bob", "by": "Ann", "byId": "a", "at": 5})
    );
}

#[test]
fn js_entry_order() {
    let o = json!({"b": 1, "10": 2, "a": 3, "2": 4, "01": 5});
    let keys: Vec<&str> = js_entries(o.as_object().unwrap())
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(keys, ["2", "10", "01", "a", "b"]);
}

#[test]
fn sizes_like_the_page() {
    assert!((checklist_width(2) - 324.0).abs() < f64::EPSILON);
    assert!((checklist_width(1) - 280.0).abs() < f64::EPSILON);
    assert!((checklist_height(2, 3, 0) - (49.0 + 28.0 + 102.0 + 14.0)).abs() < f64::EPSILON);
    assert!((checklist_height(1, 0, 0) - 97.0).abs() < f64::EPSILON);
    assert!((checklist_height(1, 0, 2) - 141.0).abs() < f64::EPSILON);
    let id = new_row_id();
    assert!(
        id.starts_with('r') && id.len() == 9 && valid_row_id(&id),
        "{id}"
    );
}

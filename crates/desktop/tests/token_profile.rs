use desktop::mcp;
use desktop::profile::{
    analyze_snapshot_text, profile_schema, render_report_table, sample_act_result,
};
use desktop_core::tree::{
    AxNodeData, RecordOutcome, SnapshotWalkBuilder, WalkBudget, find_in_snapshot,
};
use desktop_proto::{
    AppInfo, DisplayInfo, ElementAction, HostEntryStatus, HostStatus, MAX_READ_TEXT_CHARS,
    PermissionsStatus, PolicyMode, Rect, RpcResult, ScreenState, Snapshot, UiNode, WindowInfo,
    truncate_read_text,
};

#[derive(Clone)]
struct SynthNode {
    id: usize,
    data: AxNodeData,
    children: Vec<SynthNode>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn ax(
    role: &str,
    title: Option<&str>,
    value: Option<&str>,
    bounds: Rect,
    actions: Vec<ElementAction>,
) -> AxNodeData {
    AxNodeData {
        role: role.into(),
        subrole: None,
        title: title.map(str::to_string),
        label: None,
        value: value.map(str::to_string),
        bounds: Some(bounds),
        enabled: true,
        focused: false,
        selected: false,
        actions,
    }
}

fn build_dense_electron_250_fixture() -> Snapshot {
    let mut next_id = 1usize;
    let mut id = || {
        let cur = next_id;
        next_id += 1;
        cur
    };

    // Left sidebar: 35 thread cards, each with duplicate text leaves + 2 inner controls + timestamp.
    let mut left_children = vec![
        SynthNode {
            id: id(),
            data: ax(
                "button",
                Some("New thread"),
                None,
                rect(700.0, 276.0, 28.0, 28.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![],
        },
        SynthNode {
            id: id(),
            data: ax(
                "popup",
                Some("Search threads"),
                None,
                rect(520.0, 279.0, 140.0, 22.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![],
        },
    ];

    for i in 0..35 {
        let y = 316.0 + (i as f64) * 80.0;
        let title = format!("Thread task {i}, org/repo-{i}");
        left_children.push(SynthNode {
            id: id(),
            data: ax(
                "button",
                Some(&title),
                None,
                rect(488.0, y, 240.0, 78.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some(&format!("org/repo-{i}")),
                        rect(520.0, y + 10.0, 90.0, 15.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some(&format!("{i}m")),
                        rect(690.0, y + 10.0, 24.0, 15.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "popup",
                        Some("Snooze thread"),
                        None,
                        rect(634.0, y + 8.0, 25.0, 20.0),
                        vec![ElementAction::Press, ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "button",
                        Some("Settle thread"),
                        None,
                        rect(658.0, y + 8.0, 64.0, 20.0),
                        vec![ElementAction::Press, ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some(&format!("Thread task {i}")),
                        rect(498.0, y + 32.0, 180.0, 17.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
            ],
        });
    }

    // Center chat pane: 30 list items with listmarkers, headings with duplicate text, and action buttons.
    let mut center_children = Vec::new();
    for i in 0..30 {
        let y = 260.0 + (i as f64) * 40.0;
        center_children.push(SynthNode {
            id: id(),
            data: ax(
                "listmarker",
                Some("•"),
                None,
                rect(760.0, y, 8.0, 16.0),
                vec![ElementAction::ShowMenu],
            ),
            children: vec![],
        });
        center_children.push(SynthNode {
            id: id(),
            data: ax(
                "heading",
                Some(&format!("Section {i}")),
                None,
                rect(770.0, y, 240.0, 20.0),
                vec![ElementAction::ShowMenu],
            ),
            children: vec![SynthNode {
                id: id(),
                data: ax(
                    "text",
                    None,
                    Some(&format!("Section {i}")),
                    rect(770.0, y, 240.0, 20.0),
                    vec![ElementAction::ShowMenu],
                ),
                children: vec![],
            }],
        });
        center_children.push(SynthNode {
            id: id(),
            data: ax(
                "button",
                Some(&format!("Copy block {i}")),
                None,
                rect(1020.0, y, 24.0, 24.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![],
        });
    }

    // Right sidebar: version control controls including the 6-fragment Changes button.
    let mut right_children = vec![
        SynthNode {
            id: id(),
            data: ax(
                "popup",
                Some("Branch: main"),
                None,
                rect(1710.0, 402.0, 262.0, 32.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![],
        },
        SynthNode {
            id: id(),
            data: ax(
                "button",
                Some("Changes vs origin/main +0 -0"),
                None,
                rect(1710.0, 466.0, 262.0, 32.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some("Changes"),
                        rect(1747.0, 473.0, 55.0, 17.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some("vs origin/main"),
                        rect(1807.0, 473.0, 85.0, 17.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some("+"),
                        rect(1921.0, 474.0, 9.0, 16.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some("0"),
                        rect(1929.0, 474.0, 9.0, 16.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some("-"),
                        rect(1945.0, 474.0, 9.0, 16.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
                SynthNode {
                    id: id(),
                    data: ax(
                        "text",
                        None,
                        Some("0"),
                        rect(1953.0, 474.0, 8.0, 16.0),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![],
                },
            ],
        },
    ];
    for i in 0..20 {
        right_children.push(SynthNode {
            id: id(),
            data: ax(
                "button",
                Some(&format!("Action {i}")),
                None,
                rect(1710.0, 510.0 + (i as f64) * 26.0, 200.0, 24.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![],
        });
    }

    let root = SynthNode {
        id: id(),
        data: ax(
            "window",
            Some("Workspace / Dense Electron App"),
            None,
            rect(480.0, 214.0, 1513.0, 985.0),
            vec![ElementAction::Raise],
        ),
        children: vec![SynthNode {
            id: id(),
            data: ax(
                "group",
                Some("Workspace / Dense Electron App"),
                None,
                rect(480.0, 214.0, 1513.0, 985.0),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![SynthNode {
                id: id(),
                data: ax(
                    "webarea",
                    Some("Workspace / Dense Electron App"),
                    None,
                    rect(480.0, 214.0, 1513.0, 985.0),
                    vec![ElementAction::ShowMenu],
                ),
                children: vec![
                    SynthNode {
                        id: id(),
                        data: ax(
                            "group",
                            Some("Threads"),
                            None,
                            rect(480.0, 214.0, 257.0, 985.0),
                            vec![ElementAction::ShowMenu],
                        ),
                        children: left_children,
                    },
                    SynthNode {
                        id: id(),
                        data: ax(
                            "group",
                            Some("Transcript"),
                            None,
                            rect(737.0, 214.0, 964.0, 985.0),
                            vec![ElementAction::ShowMenu],
                        ),
                        children: center_children,
                    },
                    SynthNode {
                        id: id(),
                        data: ax(
                            "group",
                            Some("Thread details"),
                            None,
                            rect(1701.0, 278.0, 280.0, 600.0),
                            vec![ElementAction::ShowMenu],
                        ),
                        children: right_children,
                    },
                ],
            }],
        }],
    };

    let fallback = UiNode {
        ref_id: "@e1".into(),
        role: "window".into(),
        subrole: None,
        title: Some("Workspace / Dense Electron App".into()),
        label: None,
        value: None,
        bounds: Some(rect(480.0, 214.0, 1513.0, 985.0)),
        enabled: true,
        focused: false,
        selected: false,
        actions: vec![],
        children: vec![],
    };

    let mut builder = SnapshotWalkBuilder::new(root, WalkBudget::new(24, 250));
    while let Some(pending) = builder.next_pending() {
        let (node, meta) = pending.into_parts();
        let depth = meta.depth();
        let children = node.children;
        match builder.record_node(meta, node.data, Some(node.id)) {
            RecordOutcome::Expand(idx) => builder.enqueue_children(idx, depth + 1, children),
            RecordOutcome::CheckChildrenForTruncation => {
                if !children.is_empty() {
                    builder.mark_truncated();
                }
            }
            RecordOutcome::Done => {}
        }
    }
    let out = builder.finish(fallback);
    Snapshot {
        snapshot_id: "s1".into(),
        app: AppInfo {
            name: "Electron Demo".into(),
            bundle_id: Some("com.example.electron".into()),
            pid: Some(1234),
            path: None,
            running: true,
        },
        window_id: Some(9001),
        window_title: Some("Workspace / Dense Electron App".into()),
        window_bounds: Some(rect(480.0, 214.0, 1513.0, 985.0)),
        truncated: out.truncated,
        total_nodes: out.total_visited,
        emitted_nodes: out.emitted_nodes,
        elapsed_ms: 12,
        root: out.root,
    }
}

#[test]
fn offline_mcp_token_budgets_stay_under_ceilings() {
    // 1. Schema & instructions ceilings.
    let schema = profile_schema();
    assert!(
        schema.instructions_bytes < 550,
        "instructions={} B exceeded 550 B",
        schema.instructions_bytes
    );
    assert!(
        schema.tools_total_bytes < 6_700,
        "tools/list={} B exceeded 6700 B",
        schema.tools_total_bytes
    );
    let act_metric = schema
        .tools
        .iter()
        .find(|t| t.name == "act")
        .expect("act tool present in schema profile");
    // Baseline act schema was 3,015 B; >= 25% reduction requires <= 2,261 B.
    assert!(
        act_metric.total_bytes <= 2_260,
        "act schema={} B exceeded 2260 B (>= 25% reduction from 3015 B baseline)",
        act_metric.total_bytes
    );

    // Verify snapshot anatomy handles escaped quotes accurately.
    let escaped_snap = "snapshot_id: s1 · app: Demo\n@e1 button \"Say \\\"hi\\\"\"\n @e2 text \"Say \\\"hi\\\"\"\n";
    let escaped_anatomy = analyze_snapshot_text(escaped_snap, 2, 2, false);
    assert_eq!(escaped_anatomy.duplicate_child_text_lines, 1);

    // 2. Dense 250-node Electron snapshot ceiling (< 8 KB target, strictly < 7.5 KB here).
    let snap = build_dense_electron_250_fixture();
    let rendered = snap.render_compact();
    let anatomy = analyze_snapshot_text(
        &rendered,
        snap.emitted_nodes,
        snap.total_nodes,
        snap.truncated,
    );
    assert!(
        anatomy.total_bytes < 7_500,
        "dense Electron snapshot ({} B, {} nodes) exceeded 7500 B ceiling",
        anatomy.total_bytes,
        anatomy.emitted_nodes
    );
    assert_eq!(anatomy.show_menu_lines, 0);
    assert_eq!(anatomy.bounds_bytes, 0);
    assert_eq!(anatomy.duplicate_child_text_lines, 0);
    assert!(!rendered.contains("webarea"));
    assert!(!rendered.contains("listmarker"));
    // Verify controls across all three panes survived pruning.
    assert!(rendered.contains("button \"New thread\""));
    assert!(rendered.contains("button \"Copy block 0\""));
    assert!(rendered.contains("button \"Changes vs origin/main +0 -0\""));

    // 3. Compact find output ceiling.
    let matches = find_in_snapshot(&snap, "Thread", None, 5);
    let find_blocks = mcp::render_result(RpcResult::Find {
        snapshot_id: snap.snapshot_id.clone(),
        matches,
    });
    let find_txt = find_blocks[0].as_text().unwrap().text.as_str();
    assert!(
        find_txt.len() < 500,
        "find(5)={} B exceeded 500 B: {find_txt}",
        find_txt.len()
    );
    assert!(!find_txt.contains("window:"));
    assert!(!find_txt.contains("show_menu"));

    // 4. Compact list_windows (10 windows + 2 displays) ceiling.
    let displays = vec![
        DisplayInfo {
            index: 0,
            id: 1,
            main: true,
            bounds: rect(0.0, 0.0, 2560.0, 1440.0),
            pixel_width: 2560,
            pixel_height: 1440,
            scale: 1.0,
        },
        DisplayInfo {
            index: 1,
            id: 2,
            main: false,
            bounds: rect(-1728.0, 0.0, 1728.0, 1117.0),
            pixel_width: 3456,
            pixel_height: 2234,
            scale: 2.0,
        },
    ];
    let windows: Vec<WindowInfo> = (0..10)
        .map(|i| WindowInfo {
            id: 1000 + i,
            title: format!("Project Window {i}"),
            app: AppInfo {
                name: format!("App {i}"),
                bundle_id: Some(format!("com.example.app{i}")),
                pid: Some(500 + i),
                path: Some(format!("/Applications/App {i}.app")),
                running: true,
            },
            bounds: rect(100.0, 100.0, 1200.0, 800.0),
            layer: 0,
            alpha: 1.0,
            focused: i == 0,
            minimized: false,
            on_screen: true,
            display_index: Some(0),
        })
        .collect();
    let lw_blocks = mcp::render_result(RpcResult::Windows { displays, windows });
    let lw_txt = lw_blocks[0].as_text().unwrap().text.as_str();
    assert!(
        lw_txt.len() < 1_100,
        "list_windows(10)={} B exceeded 1100 B:\n{lw_txt}",
        lw_txt.len()
    );
    assert!(lw_txt.contains("com.example.app0"));
    assert!(lw_txt.contains("d0"));
    assert!(lw_txt.contains("focused"));
    assert!(!lw_txt.contains("/Applications/"));

    // 5. Compact list_hosts & status & act sample ceilings.
    let hosts_txt = mcp::render_hosts(&[
        HostEntryStatus {
            alias: "local".into(),
            addr: "/private/path/desktopd.sock".into(),
            reachable: true,
            latency_ms: 8,
            status: Some(HostStatus {
                host: "test-mac".into(),
                os: "macos".into(),
                version: "0.2.0".into(),
                screen_state: ScreenState::Unlocked,
                permissions: PermissionsStatus {
                    accessibility: true,
                    screen_recording: true,
                },
                policy_mode: PolicyMode::Open,
                active_lease: None,
                displays: 2,
            }),
            error: None,
        },
        HostEntryStatus {
            alias: "peer".into(),
            addr: "100.64.0.1:47281".into(),
            reachable: false,
            latency_ms: 120,
            status: None,
            error: Some("connection refused".into()),
        },
    ]);
    assert!(hosts_txt.len() < 300, "list_hosts={} B", hosts_txt.len());
    assert!(!hosts_txt.contains("/private/path/"));

    let act_blocks = mcp::render_result(RpcResult::Act(Box::new(sample_act_result())));
    let act_txt = act_blocks[0].as_text().unwrap().text.as_str();
    assert!(act_txt.len() < 550, "sample act={} B", act_txt.len());
    assert!(!act_txt.contains("(background)"));

    // 6. Report table renders cleanly without panicking.
    let table = render_report_table(&desktop::profile::TokenProfileReport {
        host: "mock".into(),
        schema,
        stages: vec![],
        snapshot_anatomy: Some(anatomy),
    });
    assert!(table.contains("Stage 0: MCP Schema"));
    assert!(table.contains("Snapshot Anatomy"));

    // 7. read_text cap constant enforced.
    let huge = "x".repeat(MAX_READ_TEXT_CHARS + 500);
    let capped = truncate_read_text(huge, MAX_READ_TEXT_CHARS);
    assert!(capped.contains("[truncated 500 chars]"));
}

#[test]
fn finder_table_snapshot_flattens_anonymous_cells_without_truncation() {
    let mut next_id = 1usize;
    let mut id = || {
        let cur = next_id;
        next_id += 1;
        cur
    };

    // Simulate a 32-row Finder list view with 110+ anonymous AXCell wrappers.
    let mut rows = Vec::new();
    for i in 0..32 {
        let y = 100.0 + (i as f64) * 20.0;
        let name_cell = SynthNode {
            id: id(),
            data: ax(
                "cell",
                None,
                None,
                rect(20.0, y, 220.0, 20.0),
                vec![ElementAction::ShowMenu],
            ),
            children: vec![SynthNode {
                id: id(),
                data: ax(
                    "textfield",
                    None,
                    Some(&format!("Document-{i}.pdf")),
                    rect(36.0, y + 2.0, 190.0, 16.0),
                    vec![ElementAction::SetValue, ElementAction::ShowMenu],
                ),
                children: vec![],
            }],
        };
        let date_cell = SynthNode {
            id: id(),
            data: ax(
                "cell",
                None,
                None,
                rect(240.0, y, 120.0, 20.0),
                vec![ElementAction::ShowMenu],
            ),
            children: vec![SynthNode {
                id: id(),
                data: ax(
                    "text",
                    None,
                    Some("Today at 10:00"),
                    rect(244.0, y + 2.0, 110.0, 16.0),
                    vec![ElementAction::ShowMenu],
                ),
                children: vec![],
            }],
        };
        let size_cell = SynthNode {
            id: id(),
            data: ax(
                "cell",
                None,
                None,
                rect(360.0, y, 80.0, 20.0),
                vec![ElementAction::ShowMenu],
            ),
            children: vec![SynthNode {
                id: id(),
                data: ax(
                    "text",
                    None,
                    Some(&format!("{i} KB")),
                    rect(364.0, y + 2.0, 70.0, 16.0),
                    vec![],
                ),
                children: vec![],
            }],
        };
        let empty_kind_cell = SynthNode {
            id: id(),
            data: ax("cell", None, None, rect(440.0, y, 80.0, 20.0), vec![]),
            children: vec![],
        };
        rows.push(SynthNode {
            id: id(),
            data: ax(
                "row",
                None,
                None,
                rect(16.0, y, 504.0, 20.0),
                vec![ElementAction::Press],
            ),
            children: vec![name_cell, date_cell, size_cell, empty_kind_cell],
        });
    }

    let root = SynthNode {
        id: id(),
        data: ax(
            "window",
            Some("Finder"),
            None,
            rect(0.0, 0.0, 900.0, 800.0),
            vec![ElementAction::Raise],
        ),
        children: vec![SynthNode {
            id: id(),
            data: ax(
                "tree",
                Some("List View"),
                None,
                rect(0.0, 60.0, 900.0, 740.0),
                vec![],
            ),
            children: rows,
        }],
    };

    let fallback = UiNode {
        ref_id: "@e1".into(),
        role: "window".into(),
        subrole: None,
        title: Some("Finder".into()),
        label: None,
        value: None,
        bounds: Some(rect(0.0, 0.0, 900.0, 800.0)),
        enabled: true,
        focused: false,
        selected: false,
        actions: vec![],
        children: vec![],
    };

    let mut builder = SnapshotWalkBuilder::new(root, WalkBudget::new(12, 250));
    while let Some(pending) = builder.next_pending() {
        let (node, meta) = pending.into_parts();
        let depth = meta.depth();
        let children = node.children;
        match builder.record_node(meta, node.data, Some(node.id)) {
            RecordOutcome::Expand(idx) => builder.enqueue_children(idx, depth + 1, children),
            RecordOutcome::CheckChildrenForTruncation => {
                if !children.is_empty() {
                    builder.mark_truncated();
                }
            }
            RecordOutcome::Done => {}
        }
    }
    let out = builder.finish(fallback);
    let snap = Snapshot {
        snapshot_id: "s-finder".into(),
        app: AppInfo {
            name: "Finder".into(),
            bundle_id: Some("com.apple.finder".into()),
            pid: Some(42),
            path: None,
            running: true,
        },
        window_id: Some(77),
        window_title: Some("Finder".into()),
        window_bounds: Some(rect(0.0, 0.0, 900.0, 800.0)),
        truncated: out.truncated,
        total_nodes: out.total_visited,
        emitted_nodes: out.emitted_nodes,
        elapsed_ms: 8,
        root: out.root,
    };

    let rendered = snap.render_compact();
    let anatomy = analyze_snapshot_text(
        &rendered,
        snap.emitted_nodes,
        snap.total_nodes,
        snap.truncated,
    );
    assert!(
        !snap.truncated,
        "Finder table should not truncate at 250 nodes"
    );
    assert_eq!(anatomy.roles.get("cell").copied().unwrap_or(0), 0);
    assert_eq!(anatomy.roles.get("row").copied().unwrap_or(0), 32);
    assert_eq!(anatomy.roles.get("textfield").copied().unwrap_or(0), 32);
    assert!(rendered.contains("Document-0.pdf"));
    assert!(rendered.contains("Document-31.pdf"));
}

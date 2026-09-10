use super::{EditOption, RowProvenance, edit_options};

fn manual() -> RowProvenance {
    RowProvenance {
        classification: "manual".to_string(),
        ..Default::default()
    }
}

#[test]
fn a_tool_run_value_is_reopened_in_its_tool_and_never_corrected_in_place() {
    let p = RowProvenance {
        has_tool_run: true,
        ..manual()
    };
    let options = edit_options(&p);
    assert!(options.contains(&EditOption::ReopenRun));
    assert!(options.contains(&EditOption::Detach));
    assert!(
        !options.contains(&EditOption::ValueCorrection),
        "correcting it here would leave the run beside a number it did not produce"
    );
    // The measurement's own state is still a person's to rule on.
    assert!(options.contains(&EditOption::Flag));
    assert!(options.contains(&EditOption::Withdraw));
}

#[test]
fn a_detached_slot_is_corrected_in_place_and_offers_the_way_back() {
    let p = RowProvenance {
        has_tool_run: true,
        slot_detached: true,
        ..manual()
    };
    let options = edit_options(&p);
    assert!(
        options.contains(&EditOption::ValueCorrection),
        "the slot is off its calculation, so the value is a person's to write"
    );
    assert!(options.contains(&EditOption::Return));
    assert!(!options.contains(&EditOption::ReopenRun));
    assert!(
        !options.contains(&EditOption::Detach),
        "it is already detached, so detaching again is refused"
    );
}

#[test]
fn a_value_no_tool_produced_is_corrected_in_place() {
    let options = edit_options(&manual());
    assert!(options.contains(&EditOption::ValueCorrection));
    assert!(!options.contains(&EditOption::ReopenRun));
    assert!(!options.contains(&EditOption::Detach));
    // Nothing corrected it, so there is no calibration window to point at; the instrument
    // still comes from a deployment, and with none covering it the fix is to create one.
    assert!(!options.contains(&EditOption::Curve));
    assert!(!options.contains(&EditOption::EditCalibration));
    assert!(options.contains(&EditOption::EditDeployment));
}

#[test]
fn each_correction_offers_the_curve_that_made_it() {
    let curved = RowProvenance {
        has_standard_curve: true,
        ..manual()
    };
    assert!(edit_options(&curved).contains(&EditOption::Curve));
    let windowed = RowProvenance {
        has_calibration: true,
        has_deployment: true,
        ..manual()
    };
    let options = edit_options(&windowed);
    assert!(
        options.contains(&EditOption::EditCalibration),
        "a windowed correction is fixed in the window, not pinned on the row"
    );
    assert!(
        options.contains(&EditOption::EditDeployment),
        "a deployed instrument is fixed in the deployment, not pinned on the row"
    );
}

#[test]
fn the_state_a_row_is_in_decides_which_half_of_each_pair_is_offered() {
    let flagged = RowProvenance {
        is_flagged: true,
        ..manual()
    };
    let options = edit_options(&flagged);
    assert!(options.contains(&EditOption::Unflag) && !options.contains(&EditOption::Flag));
    let withdrawn = RowProvenance {
        withdrawn: true,
        ..manual()
    };
    let options = edit_options(&withdrawn);
    assert!(options.contains(&EditOption::Reassert) && !options.contains(&EditOption::Withdraw));
    // A pending entry is ruled on rather than edited around.
    let pending = RowProvenance {
        unverified: true,
        ..manual()
    };
    let options = edit_options(&pending);
    assert!(options.contains(&EditOption::Verify) && options.contains(&EditOption::Reject));
    assert!(!edit_options(&manual()).contains(&EditOption::Verify));
}

#[test]
fn attribution_needs_more_than_curation_does() {
    use crate::common::authz::Capability;
    for option in [
        EditOption::ValueCorrection,
        EditOption::Flag,
        EditOption::Withdraw,
    ] {
        assert_eq!(option.capability(), Capability::WriteData, "{option:?}");
    }
    for option in [
        EditOption::Curve,
        EditOption::EditCalibration,
        EditOption::EditDeployment,
    ] {
        assert_eq!(option.capability(), Capability::ManageSensors, "{option:?}");
    }
    assert_eq!(EditOption::Detach.capability(), Capability::Admin);
}

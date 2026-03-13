/*
 * River Data Toolbox
 *
 * A Rust library for hydrochemistry calculations used in river/stream
 * monitoring: DOC, DIC, pCO2, alkalinity, nutrients, ions, chlorophyll,
 * TSS/AFDM, DOM indices, isotopes, and benthic normalizations.
 *
 * Ported from the CNET/METALP R calculation functions.
 *
 * This program is free software; you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation; either version 2 of the License, or
 * (at your option) any later version.
 */

pub mod alkalinity;
pub mod benthic;
pub mod chlorophyll;
pub mod co2_air;
pub mod common;
pub mod dic;
pub mod doc;
pub mod dom;
pub mod field_data;
pub mod ions;
pub mod isotopes;
pub mod nutrients;
pub mod pco2;
pub mod tss_afdm;

pub use alkalinity::*;
pub use benthic::*;
pub use chlorophyll::*;
pub use co2_air::*;
pub use common::*;
pub use dic::*;
pub use doc::*;
pub use dom::*;
pub use field_data::*;
pub use ions::*;
pub use isotopes::*;
pub use nutrients::*;
pub use pco2::*;
pub use tss_afdm::*;

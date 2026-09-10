// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Clients for the Textile venue. [`indexer`] is the GraphQL API the bot posts
//! orders to and reads the book from, [`submit`] is the wire form of a signed
//! order it expects, and [`enroll`] is the handshake that registers a maker
//! and hands back its RFQ stream URL and API key.

pub mod enroll;
pub mod indexer;
pub mod submit;

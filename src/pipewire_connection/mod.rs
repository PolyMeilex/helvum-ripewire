// Copyright 2021 Tom A. Wagner <tom.a.wagner@protonmail.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License version 3 as published by
// the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-only

mod ripe;
mod state;

use crate::{GtkMessage, PipewireMessage};
use state::State;

/// The "main" function of the pipewire thread.
pub(super) fn thread_main(
    gtk_sender: async_channel::Sender<PipewireMessage>,
    pw_receiver: calloop::channel::Channel<GtkMessage>,
) {
    ripe::run(gtk_sender, pw_receiver);
}

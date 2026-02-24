use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::prelude::*;

use super::{Action, Resources, Router, View, ViewId};
use crate::{
    commands::common::EVENT_POLL_TIMEOUT,
    tui::{AppFrame, restore_terminal, setup_terminal},
};

/// Main TUI application that manages views, routing, and the event loop.
#[derive(Debug)]
pub(crate) struct App {
    router: Router,
    resources: Resources,
    show_help: bool,
}

impl App {
    /// Creates a new application with the given resources and initial view.
    pub(crate) const fn new(resources: Resources, initial_view: ViewId) -> Self {
        Self { router: Router::new(initial_view), resources, show_help: false }
    }

    /// Runs the application event loop using the given view factory.
    pub(crate) async fn run<F>(mut self, mut view_factory: F) -> Result<()>
    where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        let mut terminal = setup_terminal()?;
        let result = self.run_loop(&mut terminal, &mut view_factory).await;
        restore_terminal(&mut terminal)?;
        result
    }

    async fn run_loop<F>(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        view_factory: &mut F,
    ) -> Result<()>
    where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        let mut current_view = view_factory(self.router.current());

        loop {
            self.resources.da.poll();
            self.resources.flash.poll();
            self.resources.toasts.poll();
            self.resources.poll_sys_config();

            let action = current_view.tick(&mut self.resources);
            if self.handle_action(action, &mut current_view, view_factory) {
                break;
            }

            terminal.draw(|frame| {
                let layout = AppFrame::split_layout(frame.area(), self.show_help);
                current_view.render(frame, layout.content, &self.resources);
                AppFrame::render(
                    frame,
                    &layout,
                    self.resources.chain_name(),
                    current_view.keybindings(),
                );
                self.resources.toasts.render(frame, frame.area());
            })?;

            if event::poll(EVENT_POLL_TIMEOUT)?
                && let Event::Key(key) = event::read()?
            {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }

                let action = match key.code {
                    KeyCode::Char('?') => {
                        self.show_help = !self.show_help;
                        Action::None
                    }
                    KeyCode::Char('q') => {
                        if current_view.consumes_quit() {
                            current_view.handle_key(key, &mut self.resources)
                        } else {
                            Action::Quit
                        }
                    }
                    KeyCode::Esc => {
                        if current_view.consumes_esc() {
                            current_view.handle_key(key, &mut self.resources)
                        } else if self.router.current() == ViewId::Home {
                            Action::Quit
                        } else {
                            Action::SwitchView(ViewId::Home)
                        }
                    }
                    _ => current_view.handle_key(key, &mut self.resources),
                };

                if self.handle_action(action, &mut current_view, view_factory) {
                    break;
                }
            }
        }

        Ok(())
    }

    fn handle_action<F>(
        &mut self,
        action: Action,
        current_view: &mut Box<dyn View>,
        view_factory: &mut F,
    ) -> bool
    where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        match action {
            Action::None => false,
            Action::Quit => true,
            Action::SwitchView(view_id) => {
                self.router.switch_to(view_id);
                *current_view = view_factory(view_id);
                self.show_help = false;
                false
            }
        }
    }
}

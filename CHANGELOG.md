# Changelog

## [0.2.0](https://github.com/butlerx/flighty-wall/compare/v0.1.0...v0.2.0) (2026-09-23)


### Features

* add bounded Google Calendar intake ([97f9a63](https://github.com/butlerx/flighty-wall/commit/97f9a636660950ea6c09091dd0ddfbc6fd2790a1))
* add sanitized calendar inspection command ([be1a3b0](https://github.com/butlerx/flighty-wall/commit/be1a3b09b3c4f9c26942938b00d531cac3c91f3a))
* add the FlightWall client, its config table, and the ownership journal ([7a9c78e](https://github.com/butlerx/flighty-wall/commit/7a9c78e1c79542664f1792899d764dceb48eb9c4))
* capture the FlightWall contract from the owner's Mac ([652bcf8](https://github.com/butlerx/flighty-wall/commit/652bcf8e195db2be55da5a85093065e9e044e9e3))
* establish secure sync service foundation ([b9ad5c7](https://github.com/butlerx/flighty-wall/commit/b9ad5c7fa29db18cce5da29058a7d66d82ec05cf))
* normalize Flighty calendar events into stable flights ([b87df2c](https://github.com/butlerx/flighty-wall/commit/b87df2c4533515e6abd5284575219ca61aac6ecd))
* reconcile wanted flights against the wall with the journal as sole owner ([2ec6a7a](https://github.com/butlerx/flighty-wall/commit/2ec6a7ad51b5c3edae313a68b439b179173743f7))
* rewrite the daemon in Rust and remove the Python implementation ([5aa4c43](https://github.com/butlerx/flighty-wall/commit/5aa4c43711d55e3e27ea95f8f7c7831cb3ce138d))
* run the FlightWall capture from the Mac through mise tasks ([a4ffe0c](https://github.com/butlerx/flighty-wall/commit/a4ffe0c8622e2dc31ab67c5cc38ac4773947979b))
* run the sync engine as a one-shot command and a systemd daemon ([3ccddc2](https://github.com/butlerx/flighty-wall/commit/3ccddc27f6529d1df212e92439b76d6f2ba8a70b))
* sanitize an authorized FlightWall capture into committable fixtures ([d92d56c](https://github.com/butlerx/flighty-wall/commit/d92d56c099bc60a360856bae5ef5eb16f1f5fc26))
* track only the flights departing today ([892ca9a](https://github.com/butlerx/flighty-wall/commit/892ca9a89c333294a8b034b7411f9cfd9bee4aa2))
* treat every event on the dedicated calendar as a flight ([66beb67](https://github.com/butlerx/flighty-wall/commit/66beb6786c177bd5e6d8d7aabbe3a4a76c149a28))
* verify calendar intake against live Flighty export ([b792263](https://github.com/butlerx/flighty-wall/commit/b79226325be777f4b4ddbeb3b3bb5e029708b2d5))
* write the capture HAR headlessly on Ctrl-C ([4c43347](https://github.com/butlerx/flighty-wall/commit/4c43347cfce425c0d50cb4c2b317ae9e5750e123))


### Bug Fixes

* keep httpx request lines out of the journal ([c18a460](https://github.com/butlerx/flighty-wall/commit/c18a4604c3f4e1dbc33bf18b076879aa3966dd3d))
* want a flight number once even when it flies twice in the window ([9ca56a8](https://github.com/butlerx/flighty-wall/commit/9ca56a85db731829f3bff82e784b94cfaf6f43f7))


### Code Improvements

* parse CLI arguments with click ([097d5a0](https://github.com/butlerx/flighty-wall/commit/097d5a0d7271b094062c252d6b1d64077eb512b9))
* split calendar, flightwall, and service into submodules ([efa92b5](https://github.com/butlerx/flighty-wall/commit/efa92b508e8e00b00d37b69ba3b00e6daf71fa7e))
* validate configuration with pydantic models ([3e3122c](https://github.com/butlerx/flighty-wall/commit/3e3122cbd4fb1072cc2d6d6a108889e638ec896f))


### Documentation

* bring plans and README up to date with what has landed ([640b2d8](https://github.com/butlerx/flighty-wall/commit/640b2d8a3f678426ff60b333d2db40f08687fcf6))
* close the FlightWall capability gate from the shell probes ([d55b272](https://github.com/butlerx/flighty-wall/commit/d55b272b64e634a137012606ca7ac522002629d0))
* close the sync plan with all seven units done and verified live ([2737b01](https://github.com/butlerx/flighty-wall/commit/2737b01e1ac26bf041a9d33e5560b7f8f4c4cb6b))
* define the FlightWall capture protocol and its capability gate ([388078c](https://github.com/butlerx/flighty-wall/commit/388078c295fed911000e96565f03a76dafe42880))
* drop the build-progress table from the README ([6e43abf](https://github.com/butlerx/flighty-wall/commit/6e43abf99019812c17c912d2e1ba3723a56fbdd6))
* record the click and pydantic migration as complete ([84a667b](https://github.com/butlerx/flighty-wall/commit/84a667b2ef26958bfbda15724c9185b121869ea2))
* replace the planning trail with a FlightWall API spec and a capture guide ([7027829](https://github.com/butlerx/flighty-wall/commit/70278293018a3a7fd1514c27ff785c7b491cd4eb))
* say that only today's departures reach the wall ([9d73899](https://github.com/butlerx/flighty-wall/commit/9d73899d763b8fcf4e87d852d92c4381e2b7ecd1))

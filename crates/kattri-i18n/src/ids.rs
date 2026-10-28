//! Message ids used by the pilot screens (connect and library browse). Every id here must exist
//! in the en-US catalog; the framework tests check this.

pub const CONNECT_WORKER_UNAVAILABLE: &str = "connect.error.worker_unavailable";
pub const CONNECT_CHECKING_SERVER: &str = "connect.status.checking_server";
pub const CONNECT_INTRO: &str = "connect.intro";
pub const CONNECT_FIELD_SERVER_ADDRESS: &str = "connect.field.server_address";
pub const CONNECT_FIELD_USERNAME: &str = "connect.field.username";
pub const CONNECT_FIELD_PASSWORD: &str = "connect.field.password";
pub const CONNECT_HINT_USERNAME: &str = "connect.hint.username";
pub const CONNECT_HINT_PASSWORD: &str = "connect.hint.password";
pub const OPEN_URL_MENU: &str = "open_url.menu";
pub const OPEN_URL_TITLE: &str = "open_url.title";
pub const OPEN_URL_NOTE: &str = "open_url.note";
pub const OPEN_URL_HINT: &str = "open_url.hint";
pub const OPEN_URL_PLAY: &str = "open_url.play";
pub const OPEN_URL_CANCEL: &str = "open_url.cancel";
pub const CONNECT_OPTION_REMEMBER_SIGN_IN: &str = "connect.option.remember_sign_in";
pub const CONNECT_NOTE_REMEMBER_SIGN_IN: &str = "connect.note.remember_sign_in";
pub const CONNECT_SAVED_SIGN_IN_EXPIRED: &str = "connect.error.saved_sign_in_expired";
pub const CONNECT_BUTTON_SUBMIT: &str = "connect.button.submit";
pub const CONNECT_OPTION_ALLOW_PUBLIC_HTTP: &str = "connect.option.allow_public_http";
pub const CONNECT_WARNING_HTTP_TITLE: &str = "connect.warning.http_title";
pub const CONNECT_WARNING_HTTP_BODY: &str = "connect.warning.http_body";
pub const CONNECT_NOTE_LAN_HTTP: &str = "connect.note.lan_http";
pub const CONNECT_NOTE_BACKGROUND: &str = "connect.note.background";
pub const BROWSE_HEADING: &str = "browse.heading";
pub const BROWSE_SORT_DATE_ADDED: &str = "browse.sort.date_added";
pub const BROWSE_SORT_LABEL: &str = "browse.sort.label";
pub const BROWSE_SORT_ASCENDING: &str = "browse.sort.ascending";
pub const BROWSE_SORT_DESCENDING: &str = "browse.sort.descending";
pub const BROWSE_STATE_LOADING_FIRST_PAGE: &str = "browse.state.loading_first_page";
pub const BROWSE_ERROR_LOAD_FAILED: &str = "browse.error.load_failed";
pub const BROWSE_BUTTON_RETRY: &str = "browse.button.retry";
pub const BROWSE_BUTTON_RETRY_REQUEST: &str = "browse.button.retry_request";
pub const BROWSE_BUTTON_CLEAR_FILTERS: &str = "browse.button.clear_filters";
pub const BROWSE_BUTTON_NEXT_PAGE: &str = "browse.button.next_page";
pub const BROWSE_BUTTON_CLEAR_SEARCH: &str = "browse.button.clear_search";
pub const BROWSE_EMPTY_FILTERED_TITLE: &str = "browse.empty.filtered_title";
pub const BROWSE_EMPTY_FILTERED_HINT: &str = "browse.empty.filtered_hint";
pub const BROWSE_EMPTY_LOADED_TITLE: &str = "browse.empty.loaded_title";
pub const BROWSE_SEARCH_HINT: &str = "browse.search.hint";
pub const BROWSE_SEARCH_TOO_SHORT_TITLE: &str = "browse.search.too_short_title";
pub const BROWSE_SEARCH_TOO_SHORT_HINT: &str = "browse.search.too_short_hint";
pub const BROWSE_FILTER_UNSUPPORTED: &str = "browse.filter.unsupported";
pub const ACCESS_CONNECT_SERVER_ADDRESS: &str = "access.connect.server_address";
pub const ACCESS_CONNECT_USERNAME: &str = "access.connect.username";
pub const ACCESS_CONNECT_PASSWORD: &str = "access.connect.password";
pub const ACCESS_CONNECT_ALLOW_PUBLIC_HTTP: &str = "access.connect.allow_public_http";
pub const ACCESS_CONNECT_SUBMIT: &str = "access.connect.submit";
pub const ACCESS_BROWSE_SEARCH_FIELD: &str = "access.browse.search_field";
pub const ACCESS_BROWSE_SEARCH_CLEAR: &str = "access.browse.search_clear";
pub const ACCESS_BROWSE_SORT: &str = "access.browse.sort";
pub const ACCESS_BROWSE_SORT_BY: &str = "access.browse.sort_by";
pub const ACCESS_BROWSE_PLAYED_FILTER: &str = "access.browse.played_filter";
pub const ACCESS_BROWSE_CLEAR_FILTERS: &str = "access.browse.clear_filters";
pub const ACCESS_BROWSE_NEXT_PAGE: &str = "access.browse.next_page";
pub const ACCESS_BROWSE_RETRY: &str = "access.browse.retry";
pub const ACCESS_NAV_HIGH_CONTRAST: &str = "access.nav.high_contrast";
pub const ACCESS_NAV_DISCONNECT: &str = "access.nav.disconnect";
pub const PARENTAL_PIN_LIMITATION: &str = "parental.pin.limitation";
pub const SHELL_NAV_HOME: &str = "shell.nav.home";
pub const SHELL_PROFILE_MAIN: &str = "shell.profile.main";
pub const SHELL_BRAND_NAME: &str = "shell.brand.name";
pub const CONNECT_SAVED_SERVERS: &str = "connect.saved_servers";
pub const CONNECT_HINT_SERVER_ADDRESS: &str = "connect.hint.server_address";
pub const SHELL_SERVER_VERSION_UNKNOWN: &str = "shell.server.version_unknown";
pub const CONNECT_BUTTON_CHECK_SERVER: &str = "connect.button.check_server";
pub const CONNECT_BUTTON_CANCEL_QUICK_CONNECT: &str = "connect.button.cancel_quick_connect";
pub const CONNECT_BUTTON_USE_QUICK_CONNECT: &str = "connect.button.use_quick_connect";
pub const CONNECT_STATUS_CHECKING_QUICK_CONNECT: &str = "connect.status.checking_quick_connect";
pub const LIVE_TV_STATUS_LOADING_CHANNELS: &str = "live_tv.status.loading_channels";
pub const LIVE_TV_BUTTON_RETRY_CHANNELS: &str = "live_tv.button.retry_channels";
pub const LIVE_TV_BUTTON_RETRY_GUIDE: &str = "live_tv.button.retry_guide";
pub const LIVE_TV_BUTTON_RETRY_CHANNEL: &str = "live_tv.button.retry_channel";
pub const LIVE_TV_STATUS_LOADING_MORE: &str = "live_tv.status.loading_more";
pub const LIVE_TV_BUTTON_LOAD_MORE: &str = "live_tv.button.load_more";
pub const PHOTOS_TITLE_DEFAULT: &str = "photos.title.default";
pub const CONNECT_EYEBROW: &str = "connect.eyebrow";
pub const BROWSE_STATUS_SEARCHING: &str = "browse.status.searching";
pub const BROWSE_STATUS_LOADING_PAGE: &str = "browse.status.loading_page";
pub const SEARCH_BUTTON_BACK_TO_RESULTS: &str = "search.button.back_to_results";
pub const SEARCH_STATUS_LOADING_ALL: &str = "search.status.loading_all";
pub const SEARCH_EMPTY_NO_MATCHES: &str = "search.empty.no_matches";
pub const SEARCH_NOTICE_STALE: &str = "search.notice.stale";
pub const SEARCH_EMPTY_TITLE: &str = "search.empty.title";
pub const SEARCH_BUTTON_CLEAR: &str = "search.button.clear";
pub const SEARCH_STATUS_WAITING: &str = "search.status.waiting";
pub const LIVE_TV_STATUS_SEARCHING: &str = "live_tv.status.searching";
pub const BROWSE_KICKER_COLLECTION: &str = "browse.kicker.collection";
pub const BROWSE_KICKER_LIBRARY: &str = "browse.kicker.library";
pub const BROWSE_EMPTY_SERVER_NONE: &str = "browse.empty.server_none";
pub const BROWSE_EMPTY_LOADED_HINT: &str = "browse.empty.loaded_hint";
pub const CATEGORY_MOVIES_TITLE: &str = "category.movies.title";
pub const CATEGORY_MOVIES_DESCRIPTION: &str = "category.movies.description";
pub const CATEGORY_SERIES_TITLE: &str = "category.series.title";
pub const CATEGORY_SERIES_DESCRIPTION: &str = "category.series.description";
pub const CATEGORY_MUSIC_TITLE: &str = "category.music.title";
pub const CATEGORY_MUSIC_DESCRIPTION: &str = "category.music.description";
pub const CATEGORY_PLAYLISTS_TITLE: &str = "category.playlists.title";
pub const CATEGORY_PLAYLISTS_DESCRIPTION: &str = "category.playlists.description";
pub const CATEGORY_OTHER_TITLE: &str = "category.other.title";
pub const CATEGORY_OTHER_DESCRIPTION: &str = "category.other.description";
pub const BROWSE_EMPTY_TYPE_SERVER_HINT: &str = "browse.empty.type_server_hint";
pub const BROWSE_BUTTON_SHOW_ALL: &str = "browse.button.show_all";
pub const MUSIC_QUEUE_REFRESHING: &str = "music.queue.refreshing";
pub const MUSIC_QUEUE_RESUME: &str = "music.queue.resume";
pub const MUSIC_QUEUE_RESUME_TITLE: &str = "music.queue.resume_title";
pub const PLAYLISTS_ERROR_NOT_FOUND: &str = "playlists.error.not_found";
pub const PLAYLISTS_ERROR_FORBIDDEN: &str = "playlists.error.forbidden";
pub const PLAYLISTS_STATUS_LOADING: &str = "playlists.status.loading";
pub const PLAYLISTS_STATUS_SAVING: &str = "playlists.status.saving";
pub const BROWSE_COLLECTION_EMPTY_TITLE: &str = "browse.collection.empty_title";
pub const BROWSE_COLLECTION_EMPTY_HINT: &str = "browse.collection.empty_hint";
pub const SHELL_COUNT_ITEM: &str = "shell.count.item";
pub const SHELL_COUNT_ITEMS: &str = "shell.count.items";
pub const BROWSE_SUMMARY_RUNTIME: &str = "browse.summary.runtime";
pub const BROWSE_SUMMARY_RUNTIME_HINT: &str = "browse.summary.runtime_hint";
pub const BROWSE_FILTER_FAVORITES_ONLY: &str = "browse.filter.favorites_only";
pub const DETAIL_STATUS_LOADING: &str = "detail.status.loading";
pub const MUSIC_QUEUE_ADD: &str = "music.queue.add";
pub const MUSIC_QUEUE_PLAY_NEXT: &str = "music.queue.play_next";
pub const MUSIC_ALBUM_PLAY_FIRST: &str = "music.album.play_first";
pub const MUSIC_ALBUM_LOADING_TRACKS: &str = "music.album.loading_tracks";
pub const SERIES_HEADING_SEASONS: &str = "series.heading.seasons";
pub const SERIES_STATUS_LOADING_SEASONS: &str = "series.status.loading_seasons";
pub const SERIES_EMPTY_SEASONS: &str = "series.empty.seasons";
pub const SERIES_STATUS_LOADING_EPISODES: &str = "series.status.loading_episodes";
pub const DETAIL_STATE_FAVORITE: &str = "detail.state.favorite";
pub const DETAIL_STATE_PLAYED: &str = "detail.state.played";
pub const PLAYBACK_BUTTON_PLAY: &str = "playback.button.play";
pub const DETAIL_BUTTON_DOWNLOAD: &str = "detail.button.download";
pub const PLAYBACK_STATUS_REQUESTING: &str = "playback.status.requesting";
pub const DETAIL_BUTTON_REMOVE_FAVORITE: &str = "detail.button.remove_favorite";
pub const DETAIL_BUTTON_ADD_FAVORITE: &str = "detail.button.add_favorite";
pub const DETAIL_BUTTON_MARK_UNPLAYED: &str = "detail.button.mark_unplayed";
pub const DETAIL_BUTTON_MARK_PLAYED: &str = "detail.button.mark_played";
pub const PLAYBACK_ERROR_NOT_ALLOWED: &str = "playback.error.not_allowed";
pub const PLAYBACK_ERROR_NO_STREAM: &str = "playback.error.no_stream";
pub const PLAYBACK_ERROR_RATE_LIMITED: &str = "playback.error.rate_limited";
pub const PLAYBACK_ERROR_UNKNOWN: &str = "playback.error.unknown";
pub const PLAYBACK_ERROR_NO_SOURCE: &str = "playback.error.no_source";
pub const PLAYBACK_METHOD_DIRECT_PLAY: &str = "playback.method.direct_play";
pub const PLAYBACK_METHOD_DIRECT_STREAM: &str = "playback.method.direct_stream";
pub const PLAYBACK_METHOD_TRANSCODE: &str = "playback.method.transcode";
pub const PLAYBACK_BUTTON_START_OVER: &str = "playback.button.start_over";
pub const PLAYBACK_STATUS_PREPARING: &str = "playback.status.preparing";
pub const DETAIL_EMPTY_DETAILS: &str = "detail.empty.details";
pub const PERSON_BUTTON_BACK_TO_ITEM: &str = "person.button.back_to_item";
pub const PERSON_TITLE_DEFAULT: &str = "person.title.default";
pub const PERSON_STATUS_LOADING: &str = "person.status.loading";
pub const PERSON_EMPTY: &str = "person.empty";
pub const DETAIL_ERROR_UNAVAILABLE: &str = "detail.error.unavailable";
pub const DETAIL_BUTTON_BACK: &str = "detail.button.back";
pub const DETAIL_BUTTON_RETRY: &str = "detail.button.retry";
pub const SHELL_USER_DEFAULT: &str = "shell.user.default";
pub const SHELL_EYEBROW_CONNECTED: &str = "shell.eyebrow.connected";
pub const SHELL_SERVER_VERSION_MISSING: &str = "shell.server.version_missing";
pub const BROWSE_HINT_KEYBOARD: &str = "browse.hint.keyboard";
pub const BROWSE_ERROR_REQUEST_FAILED: &str = "browse.error.request_failed";
pub const CONNECT_SUMMARY_SERVER: &str = "connect.summary.server";
pub const LIVE_TV_ERROR_GUIDE_FAILED: &str = "live_tv.error.guide_failed";
pub const SEARCH_HEADING_MATCHING: &str = "search.heading.matching";
pub const SEARCH_EMPTY_HINT: &str = "search.empty.hint";
pub const BROWSE_EMPTY_TYPE_LOADED_TITLE: &str = "browse.empty.type_loaded_title";
pub const BROWSE_COUNT_LOADED_ITEMS: &str = "browse.count.loaded_items";
pub const BROWSE_SUMMARY_RUNTIME_VALUE: &str = "browse.summary.runtime_value";
pub const BROWSE_SUMMARY_RUNTIME_UNAVAILABLE: &str = "browse.summary.runtime_unavailable";
pub const BROWSE_SUMMARY_SOME_RUNTIMES_UNAVAILABLE: &str =
    "browse.summary.some_runtimes_unavailable";
pub const BROWSE_COUNT_LOADED: &str = "browse.count.loaded";
pub const BROWSE_COUNT_OF_TOTAL: &str = "browse.count.of_total";
pub const MUSIC_ALBUM_LOAD_FAILED: &str = "music.album.load_failed";
pub const SERIES_ERROR_LOAD_SEASONS: &str = "series.error.load_seasons";
pub const SERIES_ERROR_LOAD_SEASON: &str = "series.error.load_season";
pub const DETAIL_STATE_PLAYED_COUNT: &str = "detail.state.played_count";
pub const PLAYBACK_ERROR_NEGOTIATION: &str = "playback.error.negotiation";
pub const PLAYBACK_STATUS_READY_VIA: &str = "playback.status.ready_via";
pub const PLAYBACK_BUTTON_RESUME_AT: &str = "playback.button.resume_at";
pub const PLAYBACK_ERROR_SERVER_START: &str = "playback.error.server_start";
pub const PERSON_ERROR_LOAD: &str = "person.error.load";
pub const DETAIL_ERROR_LOAD: &str = "detail.error.load";
pub const SHELL_SUMMARY_USER_SERVER: &str = "shell.summary.user_server";
pub const BROWSE_RANGE_SHOWING: &str = "browse.range.showing";
pub const BROWSE_COUNT_TOTAL_REPORTED: &str = "browse.count.total_reported";
pub const STATUS_ACTION_SIGN_IN_AGAIN: &str = "status.action.sign_in_again";
pub const STATUS_ACTION_RETRY: &str = "status.action.retry";
pub const STATUS_ERROR_UNREACHABLE: &str = "status.error.unreachable";
pub const STATUS_ERROR_UNAVAILABLE: &str = "status.error.unavailable";
pub const STATUS_ERROR_AUTH_EXPIRED: &str = "status.error.auth_expired";
pub const STATUS_ERROR_NOT_FOUND: &str = "status.error.not_found";
pub const STATUS_ERROR_FORBIDDEN: &str = "status.error.forbidden";
pub const STATUS_ERROR_UNVERIFIED: &str = "status.error.unverified";
pub const STATUS_ERROR_UNEXPECTED: &str = "status.error.unexpected";
pub const ROW_EMPTY_NO_ITEMS: &str = "row.empty.no_items";
pub const HERO_BUTTON_MORE_INFO: &str = "hero.button.more_info";
pub const DETAIL_ARTWORK_UNAVAILABLE: &str = "detail.artwork.unavailable";
pub const DETAIL_BUTTON_MORE: &str = "detail.button.more";
pub const DETAIL_BUTTON_LESS: &str = "detail.button.less";
pub const PERSON_HEADING_CAST: &str = "person.heading.cast";
pub const DETAIL_HEADING_SIMILAR: &str = "detail.heading.similar";
pub const SERIES_EMPTY_NO_SEASONS: &str = "series.empty.no_seasons";
pub const SERIES_EMPTY_NO_EPISODES: &str = "series.empty.no_episodes";
pub const SERIES_SEASON_SPECIALS: &str = "series.season.specials";
pub const SERIES_SEASON_DEFAULT: &str = "series.season.default";
pub const MEMORY_LANGUAGE_DEFAULT: &str = "memory.language.default";
pub const MEMORY_LANGUAGE_OFF: &str = "memory.language.off";
pub const MEMORY_EMPTY: &str = "memory.empty";
pub const MEMORY_HELP: &str = "memory.help";
pub const MEMORY_BUTTON_DELETE: &str = "memory.button.delete";
pub const MEMORY_BUTTON_CLEAR_ALL: &str = "memory.button.clear_all";
pub const ARTWORK_HINT_LOAD_DETAILS: &str = "artwork.hint.load_details";
pub const ADMIN_ROW_SERVER_VERSION: &str = "admin.row.server_version";
pub const ADMIN_ROW_LIBRARIES: &str = "admin.row.libraries";
pub const ADMIN_ROW_LIBRARY: &str = "admin.row.library";
pub const ADMIN_ROW_ACTIVE_SESSIONS: &str = "admin.row.active_sessions";
pub const ADMIN_ROW_USERS: &str = "admin.row.users";
pub const ADMIN_ROW_SCHEDULED_TASKS: &str = "admin.row.scheduled_tasks";
pub const ADMIN_ROW_TASK: &str = "admin.row.task";
pub const ADMIN_TASK_MORE: &str = "admin.task.more";
pub const ADMIN_TASK_NEVER_RUN: &str = "admin.task.never_run";
pub const ADMIN_STATUS_LOADING: &str = "admin.status.loading";
pub const ADMIN_ERROR_LOAD_FAILED: &str = "admin.error.load_failed";
pub const ADMIN_HEADING_TITLE: &str = "admin.heading.title";
pub const ARTWORK_DESCRIPTION_PRIMARY: &str = "artwork.description.primary";
pub const ARTWORK_DESCRIPTION_PLACEHOLDER: &str = "artwork.description.placeholder";
pub const ARTWORK_LABEL_SUMMARY: &str = "artwork.label.summary";
pub const ROW_LABEL_NO_ITEMS: &str = "row.label.no_items";
pub const ROW_LABEL_COUNT_FOCUSED: &str = "row.label.count_focused";
pub const TIME_HOURS_MINUTES: &str = "time.hours_minutes";
pub const TIME_MINUTES: &str = "time.minutes";
pub const SERIES_SEASON_NUMBERED: &str = "series.season.numbered";
pub const MEMORY_SUMMARY: &str = "memory.summary";
pub const MEMORY_SERIES: &str = "memory.series";
pub const ADMIN_LIBRARY_UNNAMED: &str = "admin.library.unnamed";
pub const ADMIN_VALUE_NOT_REPORTED: &str = "admin.value.not_reported";
pub const HOME_LIBRARY_UNTITLED: &str = "home.library.untitled";
pub const HOME_EMPTY_NO_LIBRARIES: &str = "home.empty.no_libraries";
pub const HOME_ROW_CONTINUE_WATCHING: &str = "home.row.continue_watching";
pub const HOME_ROW_NEXT_UP: &str = "home.row.next_up";
pub const HOME_ROW_LATEST: &str = "home.row.latest";
pub const HOME_EMPTY_CONTINUE: &str = "home.empty.continue";
pub const HOME_EMPTY_NEXT_UP: &str = "home.empty.next_up";
pub const HOME_EMPTY_LATEST: &str = "home.empty.latest";
pub const HOME_BUTTON_REFRESH: &str = "home.button.refresh";
pub const HOME_HEADING_MY_MEDIA: &str = "home.heading.my_media";
pub const HOME_STATUS_LOADING_LIBRARIES: &str = "home.status.loading_libraries";
pub const HOME_STATUS_REFRESHING_LIBRARIES: &str = "home.status.refreshing_libraries";
pub const HOME_BUTTON_RETRY_MY_MEDIA: &str = "home.button.retry_my_media";
pub const HOME_HEADING_COLLECTIONS: &str = "home.heading.collections";
pub const HOME_STATUS_LOADING_COLLECTIONS: &str = "home.status.loading_collections";
pub const HOME_STATUS_REFRESHING_COLLECTIONS: &str = "home.status.refreshing_collections";
pub const HOME_BUTTON_RETRY_COLLECTIONS: &str = "home.button.retry_collections";
pub const HOME_STATUS_REFRESHING: &str = "home.status.refreshing";
pub const SEARCH_CATEGORY_MOVIES: &str = "search.category.movies";
pub const SEARCH_CATEGORY_SERIES: &str = "search.category.series";
pub const SEARCH_CATEGORY_EPISODES: &str = "search.category.episodes";
pub const SEARCH_CATEGORY_MUSIC: &str = "search.category.music";
pub const SEARCH_CATEGORY_PLAYLISTS: &str = "search.category.playlists";
pub const SEARCH_CATEGORY_PHOTOS: &str = "search.category.photos";
pub const SEARCH_CATEGORY_PEOPLE: &str = "search.category.people";
pub const SEARCH_CATEGORY_CHANNELS: &str = "search.category.channels";
pub const PROFILE_HEADING_PROFILES: &str = "profile.heading.profiles";
pub const PROFILE_BUTTON_SWITCH: &str = "profile.button.switch";
pub const PROFILE_LABEL_NEW: &str = "profile.label.new";
pub const PROFILE_BUTTON_CREATE: &str = "profile.button.create";
pub const PROFILE_LABEL_PIN: &str = "profile.label.pin";
pub const PROFILE_BUTTON_UNLOCK: &str = "profile.button.unlock";
pub const PROFILE_BUTTON_CANCEL: &str = "profile.button.cancel";
pub const HOME_STATUS_LOADING_TITLE: &str = "home.status.loading_title";
pub const HOME_BUTTON_RETRY_TITLE: &str = "home.button.retry_title";
pub const HOME_ERROR_LOAD_TITLE: &str = "home.error.load_title";
pub const SEARCH_BUTTON_SHOW_ALL: &str = "search.button.show_all";
pub const PROFILE_ERROR_DUPLICATE: &str = "profile.error.duplicate";
pub const PROFILE_NOTICE_MISSING: &str = "profile.notice.missing";
pub const PROFILE_NOTICE_NO_FOLDER: &str = "profile.notice.no_folder";
pub const PROFILE_ERROR_SWITCH: &str = "profile.error.switch";
pub const PROFILE_ERROR_RELAUNCH: &str = "profile.error.relaunch";
pub const PROFILE_ERROR_READ: &str = "profile.error.read";
pub const PROFILE_ERROR_PIN_DAMAGED: &str = "profile.error.pin_damaged";
pub const PROFILE_ERROR_NO_FOLDER: &str = "profile.error.no_folder";
pub const PROFILE_ERROR_LOCK_STATE: &str = "profile.error.lock_state";
pub const PROFILE_STATE_ACTIVE: &str = "profile.state.active";
pub const PROFILE_STATE_DEFAULT: &str = "profile.state.default";
pub const PROFILE_BUTTON_SWITCH_TO: &str = "profile.button.switch_to";
pub const PROFILE_PIN_PROMPT: &str = "profile.pin.prompt";
pub const LIVE_TV_HEADING_TITLE: &str = "live_tv.heading.title";
pub const LIVE_TV_EMPTY_NOT_SET_UP: &str = "live_tv.empty.not_set_up";
pub const LIVE_TV_HEADING_CHANNELS: &str = "live_tv.heading.channels";
pub const LIVE_TV_EMPTY_GUIDE_NOT_LOADED: &str = "live_tv.empty.guide_not_loaded";
pub const LIVE_TV_HEADING_GUIDE: &str = "live_tv.heading.guide";
pub const LIVE_TV_HELP_GUIDE_CELLS: &str = "live_tv.help.guide_cells";
pub const LIVE_TV_PROGRAM_UNAVAILABLE: &str = "live_tv.program.unavailable";
pub const LIVE_TV_LOGO_PLACEHOLDER: &str = "live_tv.logo.placeholder";
pub const LIVE_TV_BUTTON_TUNE: &str = "live_tv.button.tune";
pub const LIVE_TV_STATUS_GUIDE_UNAVAILABLE: &str = "live_tv.status.guide_unavailable";
pub const LIVE_TV_HEADING_CHANNEL_TIME: &str = "live_tv.heading.channel_time";
pub const LIVE_TV_HELP_GUIDE_CHANNEL_COLUMN: &str = "live_tv.help.guide_channel_column";
pub const LIVE_TV_CHANNEL_UNNUMBERED: &str = "live_tv.channel.unnumbered";
pub const LIVE_TV_PROGRAM_NO_INFORMATION: &str = "live_tv.program.no_information";
pub const LIVE_TV_TIME_UNAVAILABLE: &str = "live_tv.time.unavailable";
pub const LIVE_TV_LABEL_LOGO: &str = "live_tv.label.logo";
pub const LIVE_TV_LABEL_NO_LOGO: &str = "live_tv.label.no_logo";
pub const LIVE_TV_LABEL_CHANNEL_NOW: &str = "live_tv.label.channel_now";
pub const LIVE_TV_BUTTON_TUNE_TO: &str = "live_tv.button.tune_to";
pub const LIVE_TV_LABEL_NOW: &str = "live_tv.label.now";
pub const LIVE_TV_LABEL_UTC_COLUMN: &str = "live_tv.label.utc_column";
pub const LIVE_TV_LABEL_GUIDE_ROW: &str = "live_tv.label.guide_row";
pub const LIVE_TV_LABEL_NOW_TITLE: &str = "live_tv.label.now_title";
pub const LIVE_TV_LABEL_ENDS: &str = "live_tv.label.ends";
pub const LIVE_TV_LABEL_ENDS_LOCAL: &str = "live_tv.label.ends_local";
pub const LIVE_TV_LABEL_ACCESSIBLE_TIME: &str = "live_tv.label.accessible_time";
pub const LIVE_TV_LABEL_CURRENT: &str = "live_tv.label.current";
pub const MUSIC_QUEUE_HEADING: &str = "music.queue.heading";
pub const MUSIC_SHUFFLE_ON: &str = "music.shuffle.on";
pub const MUSIC_SHUFFLE_OFF: &str = "music.shuffle.off";
pub const MUSIC_SHUFFLE_HOVER: &str = "music.shuffle.hover";
pub const MUSIC_REPEAT_OFF: &str = "music.repeat.off";
pub const MUSIC_REPEAT_ALL: &str = "music.repeat.all";
pub const MUSIC_REPEAT_ONE: &str = "music.repeat.one";
pub const MUSIC_REPEAT_HOVER: &str = "music.repeat.hover";
pub const MUSIC_QUEUE_EMPTY: &str = "music.queue.empty";
pub const MUSIC_QUEUE_NOTHING_PLAYING: &str = "music.queue.nothing_playing";
pub const MUSIC_QUEUE_ACTION_PLAY_NOW: &str = "music.queue.action.play_now";
pub const MUSIC_QUEUE_ACTION_PLAY_NEXT: &str = "music.queue.action.play_next";
pub const MUSIC_QUEUE_ACTION_ADD: &str = "music.queue.action.add";
pub const MUSIC_QUEUE_ACTION_REMOVE: &str = "music.queue.action.remove";
pub const MUSIC_TRACK_UNTITLED: &str = "music.track.untitled";
pub const MUSIC_SHUFFLE_A11Y: &str = "music.shuffle.a11y";
pub const MUSIC_STATE_ON: &str = "music.state.on";
pub const MUSIC_STATE_OFF: &str = "music.state.off";
pub const MUSIC_REPEAT_MODE: &str = "music.repeat.mode";
pub const MUSIC_QUEUE_NOW_PLAYING: &str = "music.queue.now_playing";
pub const MUSIC_QUEUE_CURRENT_TRACK: &str = "music.queue.current_track";
pub const MUSIC_QUEUE_ACTION_PLAY_NOW_TITLE: &str = "music.queue.action.play_now_title";
pub const MUSIC_QUEUE_ACTION_PLAY_NEXT_TITLE: &str = "music.queue.action.play_next_title";
pub const MUSIC_QUEUE_ACTION_ADD_TITLE: &str = "music.queue.action.add_title";
pub const MUSIC_QUEUE_ACTION_REMOVE_TITLE: &str = "music.queue.action.remove_title";
pub const PHOTOS_BUTTON_PREVIOUS_PAGE: &str = "photos.button.previous_page";
pub const PHOTOS_BUTTON_NEXT_PAGE: &str = "photos.button.next_page";
pub const PHOTOS_STATUS_LOADING: &str = "photos.status.loading";
pub const PHOTOS_EMPTY_NO_PHOTOS: &str = "photos.empty.no_photos";
pub const PHOTOS_KIND_FOLDER: &str = "photos.kind.folder";
pub const PHOTOS_KIND_PHOTO: &str = "photos.kind.photo";
pub const PHOTOS_ERROR_DISPLAY: &str = "photos.error.display";
pub const PHOTOS_STATUS_LOADING_PHOTO: &str = "photos.status.loading_photo";
pub const PHOTOS_BUTTON_PREVIOUS_PHOTO: &str = "photos.button.previous_photo";
pub const PHOTOS_BUTTON_CLOSE: &str = "photos.button.close";
pub const PHOTOS_BUTTON_NEXT_PHOTO: &str = "photos.button.next_photo";
pub const PHOTOS_ERROR_LOAD_FOLDER: &str = "photos.error.load_folder";
pub const PHOTOS_A11Y_OPEN_FOLDER: &str = "photos.a11y.open_folder";
pub const PHOTOS_A11Y_OPEN_PHOTO: &str = "photos.a11y.open_photo";
pub const DOWNLOADS_STATE_QUEUED: &str = "downloads.state.queued";
pub const DOWNLOADS_STATE_NEGOTIATING: &str = "downloads.state.negotiating";
pub const DOWNLOADS_STATE_DOWNLOADING: &str = "downloads.state.downloading";
pub const DOWNLOADS_STATE_VERIFYING: &str = "downloads.state.verifying";
pub const DOWNLOADS_STATE_COMMITTED: &str = "downloads.state.committed";
pub const DOWNLOADS_STATE_PAUSED: &str = "downloads.state.paused";
pub const DOWNLOADS_STATE_CANCELLED: &str = "downloads.state.cancelled";
pub const DOWNLOADS_STATE_FAILED: &str = "downloads.state.failed";
pub const DOWNLOADS_STATE_NEEDS_AUTH: &str = "downloads.state.needs_auth";
pub const DOWNLOADS_HEADING: &str = "downloads.heading";
pub const DOWNLOADS_EMPTY: &str = "downloads.empty";
pub const DOWNLOADS_BUTTON_PAUSE: &str = "downloads.button.pause";
pub const DOWNLOADS_BUTTON_RESUME: &str = "downloads.button.resume";
pub const PLAYLISTS_ERROR_BLANK: &str = "playlists.error.blank";
pub const PLAYLISTS_ERROR_TOO_LONG: &str = "playlists.error.too_long";
pub const PLAYLISTS_HEADING_TITLE: &str = "playlists.heading.title";
pub const PLAYLISTS_EMPTY_CHOOSE: &str = "playlists.empty.choose";
pub const PLAYLISTS_HEADING_CREATE: &str = "playlists.heading.create";
pub const PLAYLISTS_HINT_NAME: &str = "playlists.hint.name";
pub const PLAYLISTS_LABEL_NAME_HELP: &str = "playlists.label.name_help";
pub const PLAYLISTS_BUTTON_CREATE: &str = "playlists.button.create";
pub const PLAYLISTS_HEADING_YOURS: &str = "playlists.heading.yours";
pub const PLAYLISTS_EMPTY_NONE: &str = "playlists.empty.none";
pub const PLAYLISTS_NOUN_PLAYLISTS: &str = "playlists.noun.playlists";
pub const PLAYLISTS_BUTTON_PLAY: &str = "playlists.button.play";
pub const PLAYLISTS_HEADING_ADD_MEDIA: &str = "playlists.heading.add_media";
pub const PLAYLISTS_BUTTON_ADD_SELECTED: &str = "playlists.button.add_selected";
pub const PLAYLISTS_HEADING_ENTRIES: &str = "playlists.heading.entries";
pub const PLAYLISTS_EMPTY_NO_MEDIA: &str = "playlists.empty.no_media";
pub const PLAYLISTS_NOUN_MEDIA_ITEMS: &str = "playlists.noun.media_items";
pub const PLAYLISTS_EMPTY_ENTRIES: &str = "playlists.empty.entries";
pub const PLAYLISTS_BUTTON_MOVE_UP: &str = "playlists.button.move_up";
pub const PLAYLISTS_BUTTON_MOVE_DOWN: &str = "playlists.button.move_down";
pub const PLAYLISTS_BUTTON_REMOVE: &str = "playlists.button.remove";
pub const PLAYLISTS_NOUN_ENTRIES: &str = "playlists.noun.entries";
pub const PLAYLISTS_CARD_COUNT: &str = "playlists.card.count";
pub const PLAYLISTS_A11Y_OPEN: &str = "playlists.a11y.open";
pub const PLAYLISTS_A11Y_SELECT_TO_ADD: &str = "playlists.a11y.select_to_add";
pub const PLAYLISTS_A11Y_MOVE_UP: &str = "playlists.a11y.move_up";
pub const PLAYLISTS_A11Y_MOVE_DOWN: &str = "playlists.a11y.move_down";
pub const PLAYLISTS_A11Y_REMOVE: &str = "playlists.a11y.remove";
pub const PLAYLISTS_LIMIT_OMITTED: &str = "playlists.limit.omitted";
pub const UPDATE_BUTTON_ROLL_BACK: &str = "update.button.roll_back";
pub const UPDATE_HEADING_TITLE: &str = "update.heading.title";
pub const UPDATE_HELP_NO_DOWNLOAD: &str = "update.help.no_download";
pub const UPDATE_FIELD_ENVELOPE: &str = "update.field.envelope";
pub const UPDATE_FIELD_STAGED: &str = "update.field.staged";
pub const UPDATE_FIELD_STORE_DIR: &str = "update.field.store_dir";
pub const UPDATE_BUTTON_INSTALL_FILE: &str = "update.button.install_file";
pub const UPDATE_ERROR_CHOOSE_STAGED: &str = "update.error.choose_staged";
pub const UPDATE_ERROR_NO_FOLDER: &str = "update.error.no_folder";
pub const UPDATE_ERROR_ENVELOPE_OPEN: &str = "update.error.envelope_open";
pub const UPDATE_ERROR_ENVELOPE_TEXT: &str = "update.error.envelope_text";
pub const UPDATE_STATUS_DEFERRED: &str = "update.status.deferred";
pub const UPDATE_STATUS_REFUSED_VERSION: &str = "update.status.refused_version";
pub const UPDATE_STATUS_REFUSED: &str = "update.status.refused";
pub const UPDATE_STATUS_APPLYING: &str = "update.status.applying";
pub const UPDATE_STATUS_APPLIED: &str = "update.status.applied";
pub const UPDATE_STATUS_FAILED_VERSION: &str = "update.status.failed_version";
pub const UPDATE_STATUS_FAILED: &str = "update.status.failed";
pub const UPDATE_STATUS_ROLLING_BACK: &str = "update.status.rolling_back";
pub const UPDATE_STATUS_ROLLED_BACK: &str = "update.status.rolled_back";
pub const UPDATE_STATUS_ROLLBACK_FAILED: &str = "update.status.rollback_failed";
pub const UPDATE_ERROR_KEYS_LOAD: &str = "update.error.keys_load";
pub const SETTINGS_TAB_GENERAL: &str = "settings.tab.general";
pub const SETTINGS_TAB_SERVER: &str = "settings.tab.server";
pub const SETTINGS_TAB_PLAYBACK: &str = "settings.tab.playback";
pub const SETTINGS_TAB_AUDIO: &str = "settings.tab.audio";
pub const SETTINGS_TAB_SUBTITLES: &str = "settings.tab.subtitles";
pub const SETTINGS_TAB_VIDEO: &str = "settings.tab.video";
pub const SETTINGS_TAB_INPUT: &str = "settings.tab.input";
pub const SETTINGS_TAB_ABOUT: &str = "settings.tab.about";
pub const SETTINGS_SECTION_GENERAL_DESCRIPTION: &str = "settings.section.general.description";
pub const SETTINGS_SECTION_SERVER_DESCRIPTION: &str = "settings.section.server.description";
pub const SETTINGS_SECTION_PLAYBACK_DESCRIPTION: &str = "settings.section.playback.description";
pub const SETTINGS_SECTION_AUDIO_DESCRIPTION: &str = "settings.section.audio.description";
pub const SETTINGS_SECTION_SUBTITLES_DESCRIPTION: &str = "settings.section.subtitles.description";
pub const SETTINGS_SECTION_VIDEO_DESCRIPTION: &str = "settings.section.video.description";
pub const SETTINGS_SECTION_INPUT_DESCRIPTION: &str = "settings.section.input.description";
pub const SETTINGS_SECTION_ABOUT_DESCRIPTION: &str = "settings.section.about.description";
pub const SETTINGS_ABOUT_APP_VERSION: &str = "settings.about.app_version";
pub const SETTINGS_ABOUT_BUILD_TARGET: &str = "settings.about.build_target";
pub const SETTINGS_ABOUT_LIBMPV: &str = "settings.about.libmpv";
pub const SETTINGS_ABOUT_LIBMPV_NOT_LOADED: &str = "settings.about.libmpv_not_loaded";
pub const SETTINGS_ABOUT_GPU: &str = "settings.about.gpu";
pub const SETTINGS_VALUE_NOT_AVAILABLE: &str = "settings.value.not_available";
pub const SETTINGS_ABOUT_DEVICE_ID_FILE: &str = "settings.about.device_id_file";
pub const SETTINGS_ABOUT_LOG_FOLDER: &str = "settings.about.log_folder";
pub const SETTINGS_VALUE_ADDRESS_UNAVAILABLE: &str = "settings.value.address_unavailable";
pub const SETTINGS_HEADING_TITLE: &str = "settings.heading.title";
pub const SETTINGS_BUTTON_SWITCH_PROFILE: &str = "settings.button.switch_profile";
pub const SETTINGS_HELP_CHOOSE_CATEGORY: &str = "settings.help.choose_category";
pub const SETTINGS_BUTTON_BACK_TO_PLAYBACK: &str = "settings.button.back_to_playback";
pub const SETTINGS_BUTTON_BACK_TO_SETTINGS: &str = "settings.button.back_to_settings";
pub const SETTINGS_MEMORY_HEADING: &str = "settings.memory.heading";
pub const SETTINGS_VALUE_UNKNOWN: &str = "settings.value.unknown";
pub const SETTINGS_HEADING_DISPLAY: &str = "settings.heading.display";
pub const SETTINGS_GENERAL_HIGH_CONTRAST: &str = "settings.general.high_contrast";
pub const SETTINGS_GENERAL_REDUCE_MOTION: &str = "settings.general.reduce_motion";
pub const SETTINGS_GENERAL_MOTION_HELP: &str = "settings.general.motion_help";
pub const SETTINGS_GENERAL_INTERFACE_SIZE: &str = "settings.general.interface_size";
pub const SETTINGS_GENERAL_INTERFACE_SIZE_STANDARD: &str =
    "settings.general.interface_size.standard";
pub const SETTINGS_GENERAL_INTERFACE_SIZE_LARGE: &str = "settings.general.interface_size.large";
pub const SETTINGS_GENERAL_INTERFACE_SIZE_TEN_FOOT: &str =
    "settings.general.interface_size.ten_foot";
pub const SETTINGS_GENERAL_INTERFACE_SIZE_HELP: &str = "settings.general.interface_size_help";
pub const SETTINGS_HEADING_DIAGNOSTICS: &str = "settings.heading.diagnostics";
pub const SETTINGS_DIAGNOSTICS_CRASH_REPORTS: &str = "settings.diagnostics.crash_reports";
pub const SETTINGS_DIAGNOSTICS_HELP: &str = "settings.diagnostics.help";
pub const SETTINGS_DIAGNOSTICS_LOG_LEVEL: &str = "settings.diagnostics.log_level";
pub const SETTINGS_DIAGNOSTICS_LOG_ERROR: &str = "settings.diagnostics.log_error";
pub const SETTINGS_DIAGNOSTICS_LOG_WARN: &str = "settings.diagnostics.log_warn";
pub const SETTINGS_DIAGNOSTICS_LOG_INFO: &str = "settings.diagnostics.log_info";
pub const SETTINGS_DIAGNOSTICS_LOG_DEBUG: &str = "settings.diagnostics.log_debug";
pub const SETTINGS_DIAGNOSTICS_LOG_HELP: &str = "settings.diagnostics.log_help";
pub const SETTINGS_HEADING_ADMIN: &str = "settings.heading.admin";
pub const SETTINGS_ADMIN_SHOW_DASHBOARD: &str = "settings.admin.show_dashboard";
pub const SETTINGS_ADMIN_HELP: &str = "settings.admin.help";
pub const SETTINGS_BUTTON_SIGN_OUT: &str = "settings.button.sign_out";
pub const SETTINGS_VALUE_NOT_SIGNED_IN: &str = "settings.value.not_signed_in";
pub const SETTINGS_BUTTON_RETURN_TO_SIGN_IN: &str = "settings.button.return_to_sign_in";
pub const SETTINGS_ABOUT_READ_ONLY: &str = "settings.about.read_only";
pub const SETTINGS_ABOUT_CURRENT_SCOPE: &str = "settings.about.current_scope";
pub const PARENTAL_ERROR_PIN_MISMATCH: &str = "parental.error.pin_mismatch";
pub const PARENTAL_ERROR_PIN_LENGTH: &str = "parental.error.pin_length";
pub const PARENTAL_HEADING_TITLE: &str = "parental.heading.title";
pub const PARENTAL_ERROR_PIN_DAMAGED: &str = "parental.error.pin_damaged";
pub const PARENTAL_STATUS_NO_PIN: &str = "parental.status.no_pin";
pub const PARENTAL_BUTTON_SET_PIN: &str = "parental.button.set_pin";
pub const PARENTAL_BUTTON_SAVE_PIN: &str = "parental.button.save_pin";
pub const PARENTAL_STATUS_PIN_SET: &str = "parental.status.pin_set";
pub const PARENTAL_LABEL_PIN: &str = "parental.label.pin";
pub const PARENTAL_BUTTON_UNLOCK: &str = "parental.button.unlock";
pub const PARENTAL_BUTTON_FORGOT_PIN: &str = "parental.button.forgot_pin";
pub const PARENTAL_STATUS_LIMIT_AFTER_PIN: &str = "parental.status.limit_after_pin";
pub const PARENTAL_STATUS_UNLOCKED: &str = "parental.status.unlocked";
pub const PARENTAL_BUTTON_CHANGE_PIN: &str = "parental.button.change_pin";
pub const PARENTAL_BUTTON_SAVE_NEW_PIN: &str = "parental.button.save_new_pin";
pub const PARENTAL_BUTTON_REMOVE_PIN: &str = "parental.button.remove_pin";
pub const PARENTAL_BUTTON_LOCK_NOW: &str = "parental.button.lock_now";
pub const PARENTAL_LABEL_NEW_PIN: &str = "parental.label.new_pin";
pub const PARENTAL_LABEL_REPEAT_PIN: &str = "parental.label.repeat_pin";
pub const PARENTAL_STATUS_SAVED: &str = "parental.status.saved";
pub const PARENTAL_STATUS_REMOVED: &str = "parental.status.removed";
pub const PARENTAL_HEADING_RATING_LIMIT: &str = "parental.heading.rating_limit";
pub const PARENTAL_RATING_LIMIT: &str = "parental.rating.limit";
pub const PARENTAL_RATING_HIGHEST: &str = "parental.rating.highest";
pub const PARENTAL_RATING_HELP: &str = "parental.rating.help";
pub const PARENTAL_ERROR_FOLDER: &str = "parental.error.folder";
pub const SETTINGS_SCOPE_TEXT: &str = "settings.scope.text";
pub const SETTINGS_LABEL_ACTIVE_PROFILE: &str = "settings.label.active_profile";
pub const SETTINGS_LABEL_ADDRESS: &str = "settings.label.address";
pub const PARENTAL_STATUS_WAIT: &str = "parental.status.wait";
pub const PARENTAL_ERROR_SAVED_LOCK_NOT_WRITTEN: &str = "parental.error.saved_lock_not_written";
pub const PARENTAL_ERROR_LOCK_NOT_WRITTEN: &str = "parental.error.lock_not_written";
pub const PARENTAL_ERROR_REMOVED_LOCK_NOT_DELETED: &str = "parental.error.removed_lock_not_deleted";
pub const PARENTAL_ERROR_LOCK_READ: &str = "parental.error.lock_read";
pub const SETTINGS_INPUT_KEY_REPEAT: &str = "settings.input.key_repeat";
pub const SETTINGS_INPUT_GAMEPAD: &str = "settings.input.gamepad";
pub const SETTINGS_INPUT_GAMEPAD_HELP: &str = "settings.input.gamepad_help";
pub const SETTINGS_INPUT_KEYBOARD_HEADING: &str = "settings.input.keyboard_heading";
pub const SETTINGS_INPUT_SAVED_HELP: &str = "settings.input.saved_help";
pub const SETTINGS_INPUT_UNBOUND: &str = "settings.input.unbound";
pub const SETTINGS_INPUT_KEYMAP_REPLACED: &str = "settings.input.keymap_replaced";
pub const SETTINGS_INPUT_EDIT_HELP: &str = "settings.input.edit_help";
pub const SETTINGS_INPUT_BUTTON_CHANGE: &str = "settings.input.button.change";
pub const SETTINGS_INPUT_BUTTON_CLEAR: &str = "settings.input.button.clear";
pub const SETTINGS_INPUT_BUTTON_ADD: &str = "settings.input.button.add";
pub const SETTINGS_ACTION_MOVE_UP: &str = "settings.action.move_up";
pub const SETTINGS_ACTION_MOVE_DOWN: &str = "settings.action.move_down";
pub const SETTINGS_ACTION_MOVE_LEFT: &str = "settings.action.move_left";
pub const SETTINGS_ACTION_MOVE_RIGHT: &str = "settings.action.move_right";
pub const SETTINGS_ACTION_SELECT: &str = "settings.action.select";
pub const SETTINGS_ACTION_BACK: &str = "settings.action.back";
pub const SETTINGS_ACTION_EXIT: &str = "settings.action.exit";
pub const SETTINGS_ACTION_HOME: &str = "settings.action.home";
pub const SETTINGS_ACTION_SEARCH: &str = "settings.action.search";
pub const SETTINGS_ACTION_SETTINGS: &str = "settings.action.settings";
pub const SETTINGS_ACTION_PLAY_PAUSE: &str = "settings.action.play_pause";
pub const SETTINGS_ACTION_PAUSE: &str = "settings.action.pause";
pub const SETTINGS_ACTION_STOP: &str = "settings.action.stop";
pub const SETTINGS_ACTION_SEEK_BACK: &str = "settings.action.seek_back";
pub const SETTINGS_ACTION_SEEK_FORWARD: &str = "settings.action.seek_forward";
pub const SETTINGS_ACTION_STEP_BACK: &str = "settings.action.step_back";
pub const SETTINGS_ACTION_STEP_FORWARD: &str = "settings.action.step_forward";
pub const SETTINGS_ACTION_NEXT_AUDIO: &str = "settings.action.next_audio";
pub const SETTINGS_ACTION_PREVIOUS_AUDIO: &str = "settings.action.previous_audio";
pub const SETTINGS_ACTION_NEXT_SUBTITLE: &str = "settings.action.next_subtitle";
pub const SETTINGS_ACTION_PREVIOUS_SUBTITLE: &str = "settings.action.previous_subtitle";
pub const SETTINGS_ACTION_SUBTITLES_TOGGLE: &str = "settings.action.subtitles_toggle";
pub const SETTINGS_ACTION_AUDIO_DELAY_LATER: &str = "settings.action.audio_delay_later";
pub const SETTINGS_ACTION_AUDIO_DELAY_EARLIER: &str = "settings.action.audio_delay_earlier";
pub const SETTINGS_ACTION_SUBTITLE_DELAY_LATER: &str = "settings.action.subtitle_delay_later";
pub const SETTINGS_ACTION_SUBTITLE_DELAY_EARLIER: &str = "settings.action.subtitle_delay_earlier";
pub const SETTINGS_ACTION_RESET_SUBTITLE_DELAY: &str = "settings.action.reset_subtitle_delay";
pub const SETTINGS_ACTION_VOLUME_UP: &str = "settings.action.volume_up";
pub const SETTINGS_ACTION_VOLUME_DOWN: &str = "settings.action.volume_down";
pub const SETTINGS_ACTION_MUTE: &str = "settings.action.mute";
pub const SETTINGS_ACTION_SPEED_UP: &str = "settings.action.speed_up";
pub const SETTINGS_ACTION_SPEED_DOWN: &str = "settings.action.speed_down";
pub const SETTINGS_ACTION_NEXT_CHAPTER: &str = "settings.action.next_chapter";
pub const SETTINGS_ACTION_PREVIOUS_CHAPTER: &str = "settings.action.previous_chapter";
pub const SETTINGS_ACTION_SCREENSHOT: &str = "settings.action.screenshot";
pub const SETTINGS_ACTION_MINIMIZE_WINDOW: &str = "settings.action.minimize_window";
pub const SETTINGS_ACTION_FULLSCREEN: &str = "settings.action.fullscreen";
pub const SETTINGS_ACTION_WEB_MODE: &str = "settings.action.web_mode";
pub const SETTINGS_ACTION_QUIT: &str = "settings.action.quit";
pub const SETTINGS_ACTION_CLOSE_WINDOW: &str = "settings.action.close_window";
pub const SETTINGS_ACTION_DEBUG_OVERLAY: &str = "settings.action.debug_overlay";
pub const SETTINGS_ACTION_PICTURE_IN_PICTURE: &str = "settings.action.picture_in_picture";
pub const SETTINGS_INPUT_SAVE_FAILED: &str = "settings.input.save_failed";
pub const SETTINGS_INPUT_UNBINDABLE_KEY: &str = "settings.input.unbindable_key";
pub const SETTINGS_INPUT_ALSO_BOUND: &str = "settings.input.also_bound";
pub const SETTINGS_INPUT_PRESS_NEW_KEY: &str = "settings.input.press_new_key";
pub const SETTINGS_MEDIA_SYNC_AUDIO_CLOCK: &str = "settings.media.sync.audio_clock";
pub const SETTINGS_MEDIA_SYNC_DISPLAY_RESAMPLE: &str = "settings.media.sync.display_resample";
pub const SETTINGS_MEDIA_SYNC_DISPLAY_ADROP: &str = "settings.media.sync.display_adrop";
pub const SETTINGS_MEDIA_AUDIO_OUTPUT_BASIC: &str = "settings.media.audio_output.basic";
pub const SETTINGS_MEDIA_AUDIO_OUTPUT_SPDIF: &str = "settings.media.audio_output.spdif";
pub const SETTINGS_MEDIA_AUDIO_OUTPUT_HDMI: &str = "settings.media.audio_output.hdmi";
pub const SETTINGS_MEDIA_LAYOUT_AUTO: &str = "settings.media.layout.auto";
pub const SETTINGS_MEDIA_LAYOUT_STEREO: &str = "settings.media.layout.stereo";
pub const SETTINGS_MEDIA_LAYOUT_SURROUND51: &str = "settings.media.layout.surround51";
pub const SETTINGS_MEDIA_LAYOUT_SURROUND71: &str = "settings.media.layout.surround71";
pub const SETTINGS_MEDIA_VOLUME_100: &str = "settings.media.volume.100";
pub const SETTINGS_MEDIA_VOLUME_150: &str = "settings.media.volume.150";
pub const SETTINGS_MEDIA_VOLUME_200: &str = "settings.media.volume.200";
pub const SETTINGS_MEDIA_VOLUME_300: &str = "settings.media.volume.300";
pub const SETTINGS_MEDIA_DECODE_AUTO: &str = "settings.media.decode.auto";
pub const SETTINGS_MEDIA_DECODE_OFF: &str = "settings.media.decode.off";
pub const SETTINGS_MEDIA_DECODE_D3D11VA: &str = "settings.media.decode.d3d11va";
pub const SETTINGS_MEDIA_DECODE_NVDEC: &str = "settings.media.decode.nvdec";
pub const SETTINGS_MEDIA_DECODE_VAAPI: &str = "settings.media.decode.vaapi";
pub const SETTINGS_MEDIA_DECODE_VIDEOTOOLBOX: &str = "settings.media.decode.videotoolbox";
pub const SETTINGS_MEDIA_DECODE_VULKAN: &str = "settings.media.decode.vulkan";
pub const SETTINGS_MEDIA_ASPECT_CUSTOM: &str = "settings.media.aspect.custom";
pub const SETTINGS_MEDIA_ASPECT_NORMAL: &str = "settings.media.aspect.normal";
pub const SETTINGS_MEDIA_ASPECT_ZOOM: &str = "settings.media.aspect.zoom";
pub const SETTINGS_MEDIA_ASPECT_FORCE4X3: &str = "settings.media.aspect.force4x3";
pub const SETTINGS_MEDIA_ASPECT_FORCE16X9: &str = "settings.media.aspect.force16x9";
pub const SETTINGS_MEDIA_ASPECT_FORCE16X9_4X3: &str = "settings.media.aspect.force16x9_4x3";
pub const SETTINGS_MEDIA_ASPECT_STRETCH: &str = "settings.media.aspect.stretch";
pub const SETTINGS_MEDIA_ASPECT_NO_SCALING: &str = "settings.media.aspect.no_scaling";
pub const SETTINGS_MEDIA_SYNC_MODE_HEADING: &str = "settings.media.sync_mode.heading";
pub const SETTINGS_MEDIA_FRAME_DROP: &str = "settings.media.frame_drop";
pub const SETTINGS_MEDIA_DEFAULT_SPEED: &str = "settings.media.default_speed";
pub const SETTINGS_MEDIA_CACHE_READ: &str = "settings.media.cache.read";
pub const SETTINGS_MEDIA_CACHE_BACK_BUFFER: &str = "settings.media.cache.back_buffer";
pub const SETTINGS_MEDIA_CACHE_READ_AHEAD: &str = "settings.media.cache.read_ahead";
pub const SETTINGS_MEDIA_INTRO_SHOW_BUTTON: &str = "settings.media.intro.show_button";
pub const SETTINGS_MEDIA_INTRO_AUTO: &str = "settings.media.intro.auto";
pub const SETTINGS_MEDIA_CREDITS_SHOW_BUTTON: &str = "settings.media.credits.show_button";
pub const SETTINGS_MEDIA_CREDITS_AUTO: &str = "settings.media.credits.auto";
pub const SETTINGS_MEDIA_CREDITS_DELAY: &str = "settings.media.credits.delay";
pub const SETTINGS_MEDIA_EXTERNAL_PLAYER: &str = "settings.media.external_player";
pub const SETTINGS_MEDIA_BITRATE: &str = "settings.media.bitrate";
pub const SETTINGS_MEDIA_CACHE_HELP: &str = "settings.media.cache.help";
pub const SETTINGS_MEDIA_AUDIO_OUTPUT_HEADING: &str = "settings.media.audio.output_heading";
pub const SETTINGS_MEDIA_AUDIO_CHANNEL_LAYOUT: &str = "settings.media.audio.channel_layout";
pub const SETTINGS_MEDIA_AUDIO_OUTPUT_DEVICE: &str = "settings.media.audio.output_device";
pub const SETTINGS_MEDIA_SOUND_HEADING: &str = "settings.media.sound.heading";
pub const SETTINGS_MEDIA_EQ_HEADING: &str = "settings.media.eq.heading";
pub const SETTINGS_MEDIA_EQ_PRESET: &str = "settings.media.eq.preset";
pub const SETTINGS_MEDIA_EQ_PRESET_FLAT: &str = "settings.media.eq.preset_flat";
pub const SETTINGS_MEDIA_EQ_PRESET_BASS: &str = "settings.media.eq.preset_bass";
pub const SETTINGS_MEDIA_EQ_PRESET_TREBLE: &str = "settings.media.eq.preset_treble";
pub const SETTINGS_MEDIA_EQ_PRESET_VOICE: &str = "settings.media.eq.preset_voice";
pub const SETTINGS_MEDIA_EQ_PRESET_CUSTOM: &str = "settings.media.eq.preset_custom";
pub const SETTINGS_MEDIA_EQ_HELP: &str = "settings.media.eq.help";
pub const SETTINGS_MEDIA_LOUDNESS_NORMALIZE: &str = "settings.media.sound.loudness_normalize";
pub const SETTINGS_MEDIA_LOUDNESS_TARGET: &str = "settings.media.sound.loudness_target";
pub const SETTINGS_MEDIA_COMPRESSION: &str = "settings.media.sound.compression";
pub const SETTINGS_MEDIA_PITCH_CORRECTION: &str = "settings.media.sound.pitch_correction";
pub const SETTINGS_MEDIA_SUBTITLE_ASS_SCALE: &str = "settings.media.subtitle.ass_scale";
pub const SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE: &str = "settings.media.subtitle.ass_override";
pub const SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE_DEFAULT: &str =
    "settings.media.subtitle.ass_override_default";
pub const SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE_HELP: &str =
    "settings.media.subtitle.ass_override_help";
pub const SETTINGS_MEDIA_AUDIO_DEVICE_AUTOMATIC: &str = "settings.media.audio.device_automatic";
pub const SETTINGS_MEDIA_AUDIO_DEVICE_HELP: &str = "settings.media.audio.device_help";
pub const SETTINGS_MEDIA_AUDIO_NORMALIZE: &str = "settings.media.audio.normalize";
pub const SETTINGS_MEDIA_AUDIO_GAPLESS: &str = "settings.media.audio.gapless";
pub const SETTINGS_MEDIA_AUDIO_EXCLUSIVE: &str = "settings.media.audio.exclusive";
pub const SETTINGS_MEDIA_AUDIO_MAX_VOLUME: &str = "settings.media.audio.max_volume";
pub const SETTINGS_MEDIA_PASSTHROUGH_HEADING: &str = "settings.media.passthrough.heading";
pub const SETTINGS_MEDIA_PASSTHROUGH_AC3: &str = "settings.media.passthrough.ac3";
pub const SETTINGS_MEDIA_PASSTHROUGH_EAC3: &str = "settings.media.passthrough.eac3";
pub const SETTINGS_MEDIA_PASSTHROUGH_DTS: &str = "settings.media.passthrough.dts";
pub const SETTINGS_MEDIA_PASSTHROUGH_DTS_HD: &str = "settings.media.passthrough.dts_hd";
pub const SETTINGS_MEDIA_PASSTHROUGH_TRUEHD: &str = "settings.media.passthrough.truehd";
pub const SETTINGS_MEDIA_SUBTITLE_FONT: &str = "settings.media.subtitle.font";
pub const SETTINGS_MEDIA_SUBTITLE_SIZE: &str = "settings.media.subtitle.size";
pub const SETTINGS_MEDIA_SUBTITLE_OUTLINE_SIZE: &str = "settings.media.subtitle.outline_size";
pub const SETTINGS_MEDIA_SUBTITLE_SECONDARY_POSITION: &str =
    "settings.media.subtitle.secondary_position";
pub const SETTINGS_MEDIA_VIDEO_HARDWARE_DECODE: &str = "settings.media.video.hardware_decode";
pub const SETTINGS_MEDIA_VIDEO_DEINTERLACE: &str = "settings.media.video.deinterlace";
pub const SETTINGS_MEDIA_VIDEO_ASPECT_RATIO: &str = "settings.media.video.aspect_ratio";
pub const SETTINGS_MEDIA_VIDEO_ALLOW_ZOOM: &str = "settings.media.video.allow_zoom";
pub const SETTINGS_MEDIA_VIDEO_TONE_MAPPING: &str = "settings.media.video.tone_mapping";
pub const SETTINGS_MEDIA_TONE_MAPPING_AUTO: &str = "settings.media.tone_mapping.auto";
pub const SETTINGS_MEDIA_TONE_MAPPING_BT2390: &str = "settings.media.tone_mapping.bt2390";
pub const SETTINGS_MEDIA_TONE_MAPPING_MOBIUS: &str = "settings.media.tone_mapping.mobius";
pub const SETTINGS_MEDIA_TONE_MAPPING_HABLE: &str = "settings.media.tone_mapping.hable";
pub const SETTINGS_MEDIA_TONE_MAPPING_REINHARD: &str = "settings.media.tone_mapping.reinhard";
pub const SETTINGS_MEDIA_TONE_MAPPING_CLIP: &str = "settings.media.tone_mapping.clip";
pub const SETTINGS_MEDIA_TONE_MAPPING_HELP: &str = "settings.media.tone_mapping_help";
pub const SETTINGS_MEDIA_VIDEO_TRANSCODE_ALL: &str = "settings.media.video.transcode_all";
pub const SETTINGS_MEDIA_UNIT_MS: &str = "settings.media.unit.ms";
pub const APP_ERROR_DOWNLOAD_QUEUE: &str = "app.error.download_queue";
pub const APP_PLAYBACK_PROGRESS_STOPPED: &str = "app.playback.progress_stopped";
pub const APP_ERROR_SETTINGS_NOT_SAVED: &str = "app.error.settings_not_saved";
pub const MUSIC_NOUN_TRACK: &str = "music.noun.track";
pub const MUSIC_NOUN_TRACKS: &str = "music.noun.tracks";
pub const MUSIC_VERB_WAS: &str = "music.verb.was";
pub const MUSIC_VERB_WERE: &str = "music.verb.were";
pub const MUSIC_QUEUE_UNAVAILABLE_REMOVED: &str = "music.queue.unavailable_removed";
pub const APP_STATUS_SIGNING_IN: &str = "app.status.signing_in";
pub const BROWSE_SORT_NAME: &str = "browse.sort.name";
pub const BROWSE_SORT_RELEASE_DATE: &str = "browse.sort.release_date";
pub const BROWSE_SORT_RATING: &str = "browse.sort.rating";
pub const BROWSE_PLAYED_ANY: &str = "browse.played.any";
pub const BROWSE_PLAYED_PLAYED: &str = "browse.played.played";
pub const BROWSE_PLAYED_UNPLAYED: &str = "browse.played.unplayed";
pub const SHELL_NAV_ALL_ITEMS: &str = "shell.nav.all_items";
pub const SHELL_NAV_MOVIES: &str = "shell.nav.movies";
pub const SHELL_NAV_SERIES: &str = "shell.nav.series";
pub const SHELL_NAV_MUSIC: &str = "shell.nav.music";
pub const SHELL_NAV_PHOTOS: &str = "shell.nav.photos";
pub const SHELL_NAV_PLAYLISTS: &str = "shell.nav.playlists";
pub const SHELL_NAV_DOWNLOADS: &str = "shell.nav.downloads";
pub const SHELL_NAV_LIVE_TV: &str = "shell.nav.live_tv";
pub const SHELL_NAV_DASHBOARD: &str = "shell.nav.dashboard";
pub const SHELL_NAV_SETTINGS: &str = "shell.nav.settings";
pub const SHELL_RAIL_HOME: &str = "shell.rail.home";
pub const SHELL_RAIL_MOVIES: &str = "shell.rail.movies";
pub const SHELL_RAIL_SERIES: &str = "shell.rail.series";
pub const SHELL_RAIL_MUSIC: &str = "shell.rail.music";
pub const SHELL_RAIL_PHOTOS: &str = "shell.rail.photos";
pub const SHELL_RAIL_PLAYLISTS: &str = "shell.rail.playlists";
pub const SHELL_RAIL_DOWNLOADS: &str = "shell.rail.downloads";
pub const SHELL_RAIL_LIVE_TV: &str = "shell.rail.live_tv";
pub const SHELL_RAIL_DASHBOARD: &str = "shell.rail.dashboard";
pub const SHELL_RAIL_SETTINGS: &str = "shell.rail.settings";
pub const APP_ERROR_WORKER_START: &str = "app.error.worker_start";
pub const MUSIC_QUEUE_INVALID_MOVED: &str = "music.queue.invalid_moved";
pub const MUSIC_QUEUE_READ_FAILED: &str = "music.queue.read_failed";
pub const PROFILE_NOTICE_TITLE: &str = "profile.notice.title";
pub const APP_BUTTON_OK: &str = "app.button.ok";
pub const PROFILE_ERROR_UNAVAILABLE: &str = "profile.error.unavailable";
pub const CONNECT_ERROR_SERVER_REQUIRED: &str = "connect.error.server_required";
pub const CONNECT_ERROR_USERNAME_REQUIRED: &str = "connect.error.username_required";
pub const APP_ERROR_QUEUE_NETWORK: &str = "app.error.queue_network";
pub const CONNECT_ERROR_CHECK_ADDRESS: &str = "connect.error.check_address";
pub const CONNECT_ERROR_QUICK_CONNECT_DISABLED: &str = "connect.error.quick_connect_disabled";
pub const CONNECT_STATUS_STARTING_QUICK_CONNECT: &str = "connect.status.starting_quick_connect";
pub const LIVE_TV_NOTICE_UNAVAILABLE: &str = "live_tv.notice.unavailable";
pub const LIVE_TV_STATUS_TUNING: &str = "live_tv.status.tuning";
pub const LIVE_TV_ERROR_QUEUE_TUNE: &str = "live_tv.error.queue_tune";
pub const LIVE_TV_ERROR_TUNE_FAILED: &str = "live_tv.error.tune_failed";
pub const LIVE_TV_NOTICE_STOPPED: &str = "live_tv.notice.stopped";
pub const MUSIC_QUEUE_FINISHED: &str = "music.queue.finished";
pub const MUSIC_STATUS_PREPARING_NEXT: &str = "music.status.preparing_next";
pub const MUSIC_STATUS_PREPARING_PREVIOUS: &str = "music.status.preparing_previous";
pub const MUSIC_QUEUE_REFRESH_FAILED: &str = "music.queue.refresh_failed";
pub const MUSIC_QUEUE_NO_SAVED_TRACKS: &str = "music.queue.no_saved_tracks";
pub const MUSIC_QUEUE_RESTORED: &str = "music.queue.restored";
pub const MUSIC_QUEUE_NOTHING_TO_RESUME: &str = "music.queue.nothing_to_resume";
pub const MUSIC_QUEUE_SAVE_FAILED: &str = "music.queue.save_failed";
pub const MUSIC_QUEUE_PERSISTENCE_UNAVAILABLE: &str = "music.queue.persistence_unavailable";
pub const PLAYLISTS_NOTICE_NO_AUDIO: &str = "playlists.notice.no_audio";
pub const PLAYLISTS_NOTICE_TOO_LARGE: &str = "playlists.notice.too_large";
pub const PLAYLISTS_STATUS_PREPARING: &str = "playlists.status.preparing";
pub const DOWNLOADS_ERROR_NO_PROFILE: &str = "downloads.error.no_profile";
pub const PLAYBACK_ERROR_NO_COMPATIBLE_SOURCE: &str = "playback.error.no_compatible_source";
pub const PLAYBACK_STATUS_PREPARING_LOCAL: &str = "playback.status.preparing_local";
pub const PLAYBACK_STATUS_OPENING_LOCAL: &str = "playback.status.opening_local";
pub const APP_ERROR_NETWORK_WORKER_UNAVAILABLE: &str = "app.error.network_worker_unavailable";
pub const MUSIC_QUEUE_REFRESH_RECONNECT: &str = "music.queue.refresh_reconnect";
pub const PLAYLISTS_NOTICE_CREATED: &str = "playlists.notice.created";
pub const PLAYLISTS_ERROR_CREATE_FAILED: &str = "playlists.error.create_failed";
pub const PLAYLISTS_NOTICE_ADDED: &str = "playlists.notice.added";
pub const PLAYLISTS_NOTICE_REMOVED: &str = "playlists.notice.removed";
pub const PLAYLISTS_NOTICE_MOVED: &str = "playlists.notice.moved";
pub const PLAYLISTS_ERROR_ADD_FAILED: &str = "playlists.error.add_failed";
pub const PLAYLISTS_ERROR_REMOVE_FAILED: &str = "playlists.error.remove_failed";
pub const PLAYLISTS_ERROR_REORDER_FAILED: &str = "playlists.error.reorder_failed";
pub const DETAIL_ERROR_WATCHED_FAILED: &str = "detail.error.watched_failed";
pub const DETAIL_ERROR_FAVORITE_FAILED: &str = "detail.error.favorite_failed";
pub const PLAYBACK_ERROR_SESSION_UPDATE: &str = "playback.error.session_update";

/// Every id in this module, for the en-US coverage test.
pub const ITEM_MENU_PLAY: &str = "item_menu.play";
pub const ITEM_MENU_PLAY_FROM_START: &str = "item_menu.play_from_start";
pub const ITEM_MENU_OPEN: &str = "item_menu.open";
pub const ITEM_MENU_GO_TO_SERIES: &str = "item_menu.go_to_series";
pub const ITEM_MENU_MARK_PLAYED: &str = "item_menu.mark_played";
pub const ITEM_MENU_MARK_UNPLAYED: &str = "item_menu.mark_unplayed";
pub const ITEM_MENU_ADD_FAVORITE: &str = "item_menu.add_favorite";
pub const ITEM_MENU_REMOVE_FAVORITE: &str = "item_menu.remove_favorite";
pub const ITEM_MENU_DOWNLOAD: &str = "item_menu.download";

pub const ALL: &[&str] = &[
    CONNECT_WORKER_UNAVAILABLE,
    CONNECT_CHECKING_SERVER,
    CONNECT_INTRO,
    CONNECT_FIELD_SERVER_ADDRESS,
    CONNECT_FIELD_USERNAME,
    CONNECT_FIELD_PASSWORD,
    CONNECT_HINT_USERNAME,
    CONNECT_HINT_PASSWORD,
    OPEN_URL_MENU,
    OPEN_URL_TITLE,
    OPEN_URL_NOTE,
    OPEN_URL_HINT,
    OPEN_URL_PLAY,
    OPEN_URL_CANCEL,
    CONNECT_OPTION_REMEMBER_SIGN_IN,
    CONNECT_NOTE_REMEMBER_SIGN_IN,
    CONNECT_SAVED_SIGN_IN_EXPIRED,
    CONNECT_BUTTON_SUBMIT,
    CONNECT_OPTION_ALLOW_PUBLIC_HTTP,
    CONNECT_WARNING_HTTP_TITLE,
    CONNECT_WARNING_HTTP_BODY,
    CONNECT_NOTE_LAN_HTTP,
    CONNECT_NOTE_BACKGROUND,
    BROWSE_HEADING,
    BROWSE_SORT_DATE_ADDED,
    BROWSE_SORT_LABEL,
    BROWSE_SORT_ASCENDING,
    BROWSE_SORT_DESCENDING,
    BROWSE_STATE_LOADING_FIRST_PAGE,
    BROWSE_ERROR_LOAD_FAILED,
    BROWSE_BUTTON_RETRY,
    BROWSE_BUTTON_RETRY_REQUEST,
    BROWSE_BUTTON_CLEAR_FILTERS,
    BROWSE_BUTTON_NEXT_PAGE,
    BROWSE_BUTTON_CLEAR_SEARCH,
    BROWSE_EMPTY_FILTERED_TITLE,
    BROWSE_EMPTY_FILTERED_HINT,
    BROWSE_EMPTY_LOADED_TITLE,
    BROWSE_SEARCH_HINT,
    BROWSE_SEARCH_TOO_SHORT_TITLE,
    BROWSE_SEARCH_TOO_SHORT_HINT,
    BROWSE_FILTER_UNSUPPORTED,
    ACCESS_CONNECT_SERVER_ADDRESS,
    ACCESS_CONNECT_USERNAME,
    ACCESS_CONNECT_PASSWORD,
    ACCESS_CONNECT_ALLOW_PUBLIC_HTTP,
    ACCESS_CONNECT_SUBMIT,
    ACCESS_BROWSE_SEARCH_FIELD,
    ACCESS_BROWSE_SEARCH_CLEAR,
    ACCESS_BROWSE_SORT,
    ACCESS_BROWSE_SORT_BY,
    ACCESS_BROWSE_PLAYED_FILTER,
    ACCESS_BROWSE_CLEAR_FILTERS,
    ACCESS_BROWSE_NEXT_PAGE,
    ACCESS_BROWSE_RETRY,
    ACCESS_NAV_HIGH_CONTRAST,
    ACCESS_NAV_DISCONNECT,
    PARENTAL_PIN_LIMITATION,
    SHELL_NAV_HOME,
    SHELL_PROFILE_MAIN,
    SHELL_BRAND_NAME,
    CONNECT_SAVED_SERVERS,
    CONNECT_HINT_SERVER_ADDRESS,
    SHELL_SERVER_VERSION_UNKNOWN,
    CONNECT_BUTTON_CHECK_SERVER,
    CONNECT_BUTTON_CANCEL_QUICK_CONNECT,
    CONNECT_BUTTON_USE_QUICK_CONNECT,
    CONNECT_STATUS_CHECKING_QUICK_CONNECT,
    LIVE_TV_STATUS_LOADING_CHANNELS,
    LIVE_TV_BUTTON_RETRY_CHANNELS,
    LIVE_TV_BUTTON_RETRY_GUIDE,
    LIVE_TV_BUTTON_RETRY_CHANNEL,
    LIVE_TV_STATUS_LOADING_MORE,
    LIVE_TV_BUTTON_LOAD_MORE,
    PHOTOS_TITLE_DEFAULT,
    CONNECT_EYEBROW,
    BROWSE_STATUS_SEARCHING,
    BROWSE_STATUS_LOADING_PAGE,
    SEARCH_BUTTON_BACK_TO_RESULTS,
    SEARCH_STATUS_LOADING_ALL,
    SEARCH_EMPTY_NO_MATCHES,
    SEARCH_NOTICE_STALE,
    SEARCH_EMPTY_TITLE,
    SEARCH_BUTTON_CLEAR,
    SEARCH_STATUS_WAITING,
    LIVE_TV_STATUS_SEARCHING,
    BROWSE_KICKER_COLLECTION,
    BROWSE_KICKER_LIBRARY,
    BROWSE_EMPTY_SERVER_NONE,
    BROWSE_EMPTY_LOADED_HINT,
    CATEGORY_MOVIES_TITLE,
    CATEGORY_MOVIES_DESCRIPTION,
    CATEGORY_SERIES_TITLE,
    CATEGORY_SERIES_DESCRIPTION,
    CATEGORY_MUSIC_TITLE,
    CATEGORY_MUSIC_DESCRIPTION,
    CATEGORY_PLAYLISTS_TITLE,
    CATEGORY_PLAYLISTS_DESCRIPTION,
    CATEGORY_OTHER_TITLE,
    CATEGORY_OTHER_DESCRIPTION,
    BROWSE_EMPTY_TYPE_SERVER_HINT,
    BROWSE_BUTTON_SHOW_ALL,
    MUSIC_QUEUE_REFRESHING,
    MUSIC_QUEUE_RESUME,
    MUSIC_QUEUE_RESUME_TITLE,
    PLAYLISTS_ERROR_NOT_FOUND,
    PLAYLISTS_ERROR_FORBIDDEN,
    PLAYLISTS_STATUS_LOADING,
    PLAYLISTS_STATUS_SAVING,
    BROWSE_COLLECTION_EMPTY_TITLE,
    BROWSE_COLLECTION_EMPTY_HINT,
    SHELL_COUNT_ITEM,
    SHELL_COUNT_ITEMS,
    BROWSE_SUMMARY_RUNTIME,
    BROWSE_SUMMARY_RUNTIME_HINT,
    BROWSE_FILTER_FAVORITES_ONLY,
    DETAIL_STATUS_LOADING,
    MUSIC_QUEUE_ADD,
    MUSIC_QUEUE_PLAY_NEXT,
    MUSIC_ALBUM_PLAY_FIRST,
    MUSIC_ALBUM_LOADING_TRACKS,
    SERIES_HEADING_SEASONS,
    SERIES_STATUS_LOADING_SEASONS,
    SERIES_EMPTY_SEASONS,
    SERIES_STATUS_LOADING_EPISODES,
    DETAIL_STATE_FAVORITE,
    DETAIL_STATE_PLAYED,
    PLAYBACK_BUTTON_PLAY,
    ITEM_MENU_PLAY,
    ITEM_MENU_PLAY_FROM_START,
    ITEM_MENU_OPEN,
    ITEM_MENU_GO_TO_SERIES,
    ITEM_MENU_MARK_PLAYED,
    ITEM_MENU_MARK_UNPLAYED,
    ITEM_MENU_ADD_FAVORITE,
    ITEM_MENU_REMOVE_FAVORITE,
    ITEM_MENU_DOWNLOAD,
    DETAIL_BUTTON_DOWNLOAD,
    PLAYBACK_STATUS_REQUESTING,
    DETAIL_BUTTON_REMOVE_FAVORITE,
    DETAIL_BUTTON_ADD_FAVORITE,
    DETAIL_BUTTON_MARK_UNPLAYED,
    DETAIL_BUTTON_MARK_PLAYED,
    PLAYBACK_ERROR_NOT_ALLOWED,
    PLAYBACK_ERROR_NO_STREAM,
    PLAYBACK_ERROR_RATE_LIMITED,
    PLAYBACK_ERROR_UNKNOWN,
    PLAYBACK_ERROR_NO_SOURCE,
    PLAYBACK_METHOD_DIRECT_PLAY,
    PLAYBACK_METHOD_DIRECT_STREAM,
    PLAYBACK_METHOD_TRANSCODE,
    PLAYBACK_BUTTON_START_OVER,
    PLAYBACK_STATUS_PREPARING,
    DETAIL_EMPTY_DETAILS,
    PERSON_BUTTON_BACK_TO_ITEM,
    PERSON_TITLE_DEFAULT,
    PERSON_STATUS_LOADING,
    PERSON_EMPTY,
    DETAIL_ERROR_UNAVAILABLE,
    DETAIL_BUTTON_BACK,
    DETAIL_BUTTON_RETRY,
    SHELL_USER_DEFAULT,
    SHELL_EYEBROW_CONNECTED,
    SHELL_SERVER_VERSION_MISSING,
    BROWSE_HINT_KEYBOARD,
    BROWSE_ERROR_REQUEST_FAILED,
    CONNECT_SUMMARY_SERVER,
    LIVE_TV_ERROR_GUIDE_FAILED,
    SEARCH_HEADING_MATCHING,
    SEARCH_EMPTY_HINT,
    BROWSE_EMPTY_TYPE_LOADED_TITLE,
    BROWSE_COUNT_LOADED_ITEMS,
    BROWSE_SUMMARY_RUNTIME_VALUE,
    BROWSE_SUMMARY_RUNTIME_UNAVAILABLE,
    BROWSE_SUMMARY_SOME_RUNTIMES_UNAVAILABLE,
    BROWSE_COUNT_LOADED,
    BROWSE_COUNT_OF_TOTAL,
    MUSIC_ALBUM_LOAD_FAILED,
    SERIES_ERROR_LOAD_SEASONS,
    SERIES_ERROR_LOAD_SEASON,
    DETAIL_STATE_PLAYED_COUNT,
    PLAYBACK_ERROR_NEGOTIATION,
    PLAYBACK_STATUS_READY_VIA,
    PLAYBACK_BUTTON_RESUME_AT,
    PLAYBACK_ERROR_SERVER_START,
    PERSON_ERROR_LOAD,
    DETAIL_ERROR_LOAD,
    SHELL_SUMMARY_USER_SERVER,
    BROWSE_RANGE_SHOWING,
    BROWSE_COUNT_TOTAL_REPORTED,
    STATUS_ACTION_SIGN_IN_AGAIN,
    STATUS_ACTION_RETRY,
    STATUS_ERROR_UNREACHABLE,
    STATUS_ERROR_UNAVAILABLE,
    STATUS_ERROR_AUTH_EXPIRED,
    STATUS_ERROR_NOT_FOUND,
    STATUS_ERROR_FORBIDDEN,
    STATUS_ERROR_UNVERIFIED,
    STATUS_ERROR_UNEXPECTED,
    ROW_EMPTY_NO_ITEMS,
    HERO_BUTTON_MORE_INFO,
    DETAIL_ARTWORK_UNAVAILABLE,
    DETAIL_BUTTON_MORE,
    DETAIL_BUTTON_LESS,
    PERSON_HEADING_CAST,
    DETAIL_HEADING_SIMILAR,
    SERIES_EMPTY_NO_SEASONS,
    SERIES_EMPTY_NO_EPISODES,
    SERIES_SEASON_SPECIALS,
    SERIES_SEASON_DEFAULT,
    MEMORY_LANGUAGE_DEFAULT,
    MEMORY_LANGUAGE_OFF,
    MEMORY_EMPTY,
    MEMORY_HELP,
    MEMORY_BUTTON_DELETE,
    MEMORY_BUTTON_CLEAR_ALL,
    ARTWORK_HINT_LOAD_DETAILS,
    ADMIN_ROW_SERVER_VERSION,
    ADMIN_ROW_LIBRARIES,
    ADMIN_ROW_LIBRARY,
    ADMIN_ROW_ACTIVE_SESSIONS,
    ADMIN_ROW_USERS,
    ADMIN_ROW_SCHEDULED_TASKS,
    ADMIN_ROW_TASK,
    ADMIN_TASK_MORE,
    ADMIN_TASK_NEVER_RUN,
    ADMIN_STATUS_LOADING,
    ADMIN_ERROR_LOAD_FAILED,
    ADMIN_HEADING_TITLE,
    ARTWORK_DESCRIPTION_PRIMARY,
    ARTWORK_DESCRIPTION_PLACEHOLDER,
    ARTWORK_LABEL_SUMMARY,
    ROW_LABEL_NO_ITEMS,
    ROW_LABEL_COUNT_FOCUSED,
    TIME_HOURS_MINUTES,
    TIME_MINUTES,
    SERIES_SEASON_NUMBERED,
    MEMORY_SUMMARY,
    MEMORY_SERIES,
    ADMIN_LIBRARY_UNNAMED,
    ADMIN_VALUE_NOT_REPORTED,
    HOME_LIBRARY_UNTITLED,
    HOME_EMPTY_NO_LIBRARIES,
    HOME_ROW_CONTINUE_WATCHING,
    HOME_ROW_NEXT_UP,
    HOME_ROW_LATEST,
    HOME_EMPTY_CONTINUE,
    HOME_EMPTY_NEXT_UP,
    HOME_EMPTY_LATEST,
    HOME_BUTTON_REFRESH,
    HOME_HEADING_MY_MEDIA,
    HOME_STATUS_LOADING_LIBRARIES,
    HOME_STATUS_REFRESHING_LIBRARIES,
    HOME_BUTTON_RETRY_MY_MEDIA,
    HOME_HEADING_COLLECTIONS,
    HOME_STATUS_LOADING_COLLECTIONS,
    HOME_STATUS_REFRESHING_COLLECTIONS,
    HOME_BUTTON_RETRY_COLLECTIONS,
    HOME_STATUS_REFRESHING,
    SEARCH_CATEGORY_MOVIES,
    SEARCH_CATEGORY_SERIES,
    SEARCH_CATEGORY_EPISODES,
    SEARCH_CATEGORY_MUSIC,
    SEARCH_CATEGORY_PLAYLISTS,
    SEARCH_CATEGORY_PHOTOS,
    SEARCH_CATEGORY_PEOPLE,
    SEARCH_CATEGORY_CHANNELS,
    PROFILE_HEADING_PROFILES,
    PROFILE_BUTTON_SWITCH,
    PROFILE_LABEL_NEW,
    PROFILE_BUTTON_CREATE,
    PROFILE_LABEL_PIN,
    PROFILE_BUTTON_UNLOCK,
    PROFILE_BUTTON_CANCEL,
    HOME_STATUS_LOADING_TITLE,
    HOME_BUTTON_RETRY_TITLE,
    HOME_ERROR_LOAD_TITLE,
    SEARCH_BUTTON_SHOW_ALL,
    PROFILE_ERROR_DUPLICATE,
    PROFILE_NOTICE_MISSING,
    PROFILE_NOTICE_NO_FOLDER,
    PROFILE_ERROR_SWITCH,
    PROFILE_ERROR_RELAUNCH,
    PROFILE_ERROR_READ,
    PROFILE_ERROR_PIN_DAMAGED,
    PROFILE_ERROR_NO_FOLDER,
    PROFILE_ERROR_LOCK_STATE,
    PROFILE_STATE_ACTIVE,
    PROFILE_STATE_DEFAULT,
    PROFILE_BUTTON_SWITCH_TO,
    PROFILE_PIN_PROMPT,
    LIVE_TV_HEADING_TITLE,
    LIVE_TV_EMPTY_NOT_SET_UP,
    LIVE_TV_HEADING_CHANNELS,
    LIVE_TV_EMPTY_GUIDE_NOT_LOADED,
    LIVE_TV_HEADING_GUIDE,
    LIVE_TV_HELP_GUIDE_CELLS,
    LIVE_TV_PROGRAM_UNAVAILABLE,
    LIVE_TV_LOGO_PLACEHOLDER,
    LIVE_TV_BUTTON_TUNE,
    LIVE_TV_STATUS_GUIDE_UNAVAILABLE,
    LIVE_TV_HEADING_CHANNEL_TIME,
    LIVE_TV_HELP_GUIDE_CHANNEL_COLUMN,
    LIVE_TV_CHANNEL_UNNUMBERED,
    LIVE_TV_PROGRAM_NO_INFORMATION,
    LIVE_TV_TIME_UNAVAILABLE,
    LIVE_TV_LABEL_LOGO,
    LIVE_TV_LABEL_NO_LOGO,
    LIVE_TV_LABEL_CHANNEL_NOW,
    LIVE_TV_BUTTON_TUNE_TO,
    LIVE_TV_LABEL_NOW,
    LIVE_TV_LABEL_UTC_COLUMN,
    LIVE_TV_LABEL_GUIDE_ROW,
    LIVE_TV_LABEL_NOW_TITLE,
    LIVE_TV_LABEL_ENDS,
    LIVE_TV_LABEL_ENDS_LOCAL,
    LIVE_TV_LABEL_ACCESSIBLE_TIME,
    LIVE_TV_LABEL_CURRENT,
    MUSIC_QUEUE_HEADING,
    MUSIC_SHUFFLE_ON,
    MUSIC_SHUFFLE_OFF,
    MUSIC_SHUFFLE_HOVER,
    MUSIC_REPEAT_OFF,
    MUSIC_REPEAT_ALL,
    MUSIC_REPEAT_ONE,
    MUSIC_REPEAT_HOVER,
    MUSIC_QUEUE_EMPTY,
    MUSIC_QUEUE_NOTHING_PLAYING,
    MUSIC_QUEUE_ACTION_PLAY_NOW,
    MUSIC_QUEUE_ACTION_PLAY_NEXT,
    MUSIC_QUEUE_ACTION_ADD,
    MUSIC_QUEUE_ACTION_REMOVE,
    MUSIC_TRACK_UNTITLED,
    MUSIC_SHUFFLE_A11Y,
    MUSIC_STATE_ON,
    MUSIC_STATE_OFF,
    MUSIC_REPEAT_MODE,
    MUSIC_QUEUE_NOW_PLAYING,
    MUSIC_QUEUE_CURRENT_TRACK,
    MUSIC_QUEUE_ACTION_PLAY_NOW_TITLE,
    MUSIC_QUEUE_ACTION_PLAY_NEXT_TITLE,
    MUSIC_QUEUE_ACTION_ADD_TITLE,
    MUSIC_QUEUE_ACTION_REMOVE_TITLE,
    PHOTOS_BUTTON_PREVIOUS_PAGE,
    PHOTOS_BUTTON_NEXT_PAGE,
    PHOTOS_STATUS_LOADING,
    PHOTOS_EMPTY_NO_PHOTOS,
    PHOTOS_KIND_FOLDER,
    PHOTOS_KIND_PHOTO,
    PHOTOS_ERROR_DISPLAY,
    PHOTOS_STATUS_LOADING_PHOTO,
    PHOTOS_BUTTON_PREVIOUS_PHOTO,
    PHOTOS_BUTTON_CLOSE,
    PHOTOS_BUTTON_NEXT_PHOTO,
    PHOTOS_ERROR_LOAD_FOLDER,
    PHOTOS_A11Y_OPEN_FOLDER,
    PHOTOS_A11Y_OPEN_PHOTO,
    DOWNLOADS_STATE_QUEUED,
    DOWNLOADS_STATE_NEGOTIATING,
    DOWNLOADS_STATE_DOWNLOADING,
    DOWNLOADS_STATE_VERIFYING,
    DOWNLOADS_STATE_COMMITTED,
    DOWNLOADS_STATE_PAUSED,
    DOWNLOADS_STATE_CANCELLED,
    DOWNLOADS_STATE_FAILED,
    DOWNLOADS_STATE_NEEDS_AUTH,
    DOWNLOADS_HEADING,
    DOWNLOADS_EMPTY,
    DOWNLOADS_BUTTON_PAUSE,
    DOWNLOADS_BUTTON_RESUME,
    PLAYLISTS_ERROR_BLANK,
    PLAYLISTS_ERROR_TOO_LONG,
    PLAYLISTS_HEADING_TITLE,
    PLAYLISTS_EMPTY_CHOOSE,
    PLAYLISTS_HEADING_CREATE,
    PLAYLISTS_HINT_NAME,
    PLAYLISTS_LABEL_NAME_HELP,
    PLAYLISTS_BUTTON_CREATE,
    PLAYLISTS_HEADING_YOURS,
    PLAYLISTS_EMPTY_NONE,
    PLAYLISTS_NOUN_PLAYLISTS,
    PLAYLISTS_BUTTON_PLAY,
    PLAYLISTS_HEADING_ADD_MEDIA,
    PLAYLISTS_BUTTON_ADD_SELECTED,
    PLAYLISTS_HEADING_ENTRIES,
    PLAYLISTS_EMPTY_NO_MEDIA,
    PLAYLISTS_NOUN_MEDIA_ITEMS,
    PLAYLISTS_EMPTY_ENTRIES,
    PLAYLISTS_BUTTON_MOVE_UP,
    PLAYLISTS_BUTTON_MOVE_DOWN,
    PLAYLISTS_BUTTON_REMOVE,
    PLAYLISTS_NOUN_ENTRIES,
    PLAYLISTS_CARD_COUNT,
    PLAYLISTS_A11Y_OPEN,
    PLAYLISTS_A11Y_SELECT_TO_ADD,
    PLAYLISTS_A11Y_MOVE_UP,
    PLAYLISTS_A11Y_MOVE_DOWN,
    PLAYLISTS_A11Y_REMOVE,
    PLAYLISTS_LIMIT_OMITTED,
    UPDATE_BUTTON_ROLL_BACK,
    UPDATE_HEADING_TITLE,
    UPDATE_HELP_NO_DOWNLOAD,
    UPDATE_FIELD_ENVELOPE,
    UPDATE_FIELD_STAGED,
    UPDATE_FIELD_STORE_DIR,
    UPDATE_BUTTON_INSTALL_FILE,
    UPDATE_ERROR_CHOOSE_STAGED,
    UPDATE_ERROR_NO_FOLDER,
    UPDATE_ERROR_ENVELOPE_OPEN,
    UPDATE_ERROR_ENVELOPE_TEXT,
    UPDATE_STATUS_DEFERRED,
    UPDATE_STATUS_REFUSED_VERSION,
    UPDATE_STATUS_REFUSED,
    UPDATE_STATUS_APPLYING,
    UPDATE_STATUS_APPLIED,
    UPDATE_STATUS_FAILED_VERSION,
    UPDATE_STATUS_FAILED,
    UPDATE_STATUS_ROLLING_BACK,
    UPDATE_STATUS_ROLLED_BACK,
    UPDATE_STATUS_ROLLBACK_FAILED,
    UPDATE_ERROR_KEYS_LOAD,
    SETTINGS_TAB_GENERAL,
    SETTINGS_TAB_SERVER,
    SETTINGS_TAB_PLAYBACK,
    SETTINGS_TAB_AUDIO,
    SETTINGS_TAB_SUBTITLES,
    SETTINGS_TAB_VIDEO,
    SETTINGS_TAB_INPUT,
    SETTINGS_TAB_ABOUT,
    SETTINGS_SECTION_GENERAL_DESCRIPTION,
    SETTINGS_SECTION_SERVER_DESCRIPTION,
    SETTINGS_SECTION_PLAYBACK_DESCRIPTION,
    SETTINGS_SECTION_AUDIO_DESCRIPTION,
    SETTINGS_SECTION_SUBTITLES_DESCRIPTION,
    SETTINGS_SECTION_VIDEO_DESCRIPTION,
    SETTINGS_SECTION_INPUT_DESCRIPTION,
    SETTINGS_SECTION_ABOUT_DESCRIPTION,
    SETTINGS_ABOUT_APP_VERSION,
    SETTINGS_ABOUT_BUILD_TARGET,
    SETTINGS_ABOUT_LIBMPV,
    SETTINGS_ABOUT_LIBMPV_NOT_LOADED,
    SETTINGS_ABOUT_GPU,
    SETTINGS_VALUE_NOT_AVAILABLE,
    SETTINGS_ABOUT_DEVICE_ID_FILE,
    SETTINGS_ABOUT_LOG_FOLDER,
    SETTINGS_VALUE_ADDRESS_UNAVAILABLE,
    SETTINGS_HEADING_TITLE,
    SETTINGS_BUTTON_SWITCH_PROFILE,
    SETTINGS_HELP_CHOOSE_CATEGORY,
    SETTINGS_BUTTON_BACK_TO_PLAYBACK,
    SETTINGS_BUTTON_BACK_TO_SETTINGS,
    SETTINGS_MEMORY_HEADING,
    SETTINGS_VALUE_UNKNOWN,
    SETTINGS_HEADING_DISPLAY,
    SETTINGS_GENERAL_HIGH_CONTRAST,
    SETTINGS_GENERAL_REDUCE_MOTION,
    SETTINGS_GENERAL_MOTION_HELP,
    SETTINGS_GENERAL_INTERFACE_SIZE,
    SETTINGS_GENERAL_INTERFACE_SIZE_STANDARD,
    SETTINGS_GENERAL_INTERFACE_SIZE_LARGE,
    SETTINGS_GENERAL_INTERFACE_SIZE_TEN_FOOT,
    SETTINGS_GENERAL_INTERFACE_SIZE_HELP,
    SETTINGS_HEADING_DIAGNOSTICS,
    SETTINGS_DIAGNOSTICS_CRASH_REPORTS,
    SETTINGS_DIAGNOSTICS_HELP,
    SETTINGS_DIAGNOSTICS_LOG_LEVEL,
    SETTINGS_DIAGNOSTICS_LOG_ERROR,
    SETTINGS_DIAGNOSTICS_LOG_WARN,
    SETTINGS_DIAGNOSTICS_LOG_INFO,
    SETTINGS_DIAGNOSTICS_LOG_DEBUG,
    SETTINGS_DIAGNOSTICS_LOG_HELP,
    SETTINGS_HEADING_ADMIN,
    SETTINGS_ADMIN_SHOW_DASHBOARD,
    SETTINGS_ADMIN_HELP,
    SETTINGS_BUTTON_SIGN_OUT,
    SETTINGS_VALUE_NOT_SIGNED_IN,
    SETTINGS_BUTTON_RETURN_TO_SIGN_IN,
    SETTINGS_ABOUT_READ_ONLY,
    SETTINGS_ABOUT_CURRENT_SCOPE,
    PARENTAL_ERROR_PIN_MISMATCH,
    PARENTAL_ERROR_PIN_LENGTH,
    PARENTAL_HEADING_TITLE,
    PARENTAL_ERROR_PIN_DAMAGED,
    PARENTAL_STATUS_NO_PIN,
    PARENTAL_BUTTON_SET_PIN,
    PARENTAL_BUTTON_SAVE_PIN,
    PARENTAL_STATUS_PIN_SET,
    PARENTAL_LABEL_PIN,
    PARENTAL_BUTTON_UNLOCK,
    PARENTAL_BUTTON_FORGOT_PIN,
    PARENTAL_STATUS_LIMIT_AFTER_PIN,
    PARENTAL_STATUS_UNLOCKED,
    PARENTAL_BUTTON_CHANGE_PIN,
    PARENTAL_BUTTON_SAVE_NEW_PIN,
    PARENTAL_BUTTON_REMOVE_PIN,
    PARENTAL_BUTTON_LOCK_NOW,
    PARENTAL_LABEL_NEW_PIN,
    PARENTAL_LABEL_REPEAT_PIN,
    PARENTAL_STATUS_SAVED,
    PARENTAL_STATUS_REMOVED,
    PARENTAL_HEADING_RATING_LIMIT,
    PARENTAL_RATING_LIMIT,
    PARENTAL_RATING_HIGHEST,
    PARENTAL_RATING_HELP,
    PARENTAL_ERROR_FOLDER,
    SETTINGS_SCOPE_TEXT,
    SETTINGS_LABEL_ACTIVE_PROFILE,
    SETTINGS_LABEL_ADDRESS,
    PARENTAL_STATUS_WAIT,
    PARENTAL_ERROR_SAVED_LOCK_NOT_WRITTEN,
    PARENTAL_ERROR_LOCK_NOT_WRITTEN,
    PARENTAL_ERROR_REMOVED_LOCK_NOT_DELETED,
    PARENTAL_ERROR_LOCK_READ,
    SETTINGS_INPUT_KEY_REPEAT,
    SETTINGS_INPUT_GAMEPAD,
    SETTINGS_INPUT_GAMEPAD_HELP,
    SETTINGS_INPUT_KEYBOARD_HEADING,
    SETTINGS_INPUT_SAVED_HELP,
    SETTINGS_INPUT_UNBOUND,
    SETTINGS_INPUT_KEYMAP_REPLACED,
    SETTINGS_INPUT_EDIT_HELP,
    SETTINGS_INPUT_BUTTON_CHANGE,
    SETTINGS_INPUT_BUTTON_CLEAR,
    SETTINGS_INPUT_BUTTON_ADD,
    SETTINGS_ACTION_MOVE_UP,
    SETTINGS_ACTION_MOVE_DOWN,
    SETTINGS_ACTION_MOVE_LEFT,
    SETTINGS_ACTION_MOVE_RIGHT,
    SETTINGS_ACTION_SELECT,
    SETTINGS_ACTION_BACK,
    SETTINGS_ACTION_EXIT,
    SETTINGS_ACTION_HOME,
    SETTINGS_ACTION_SEARCH,
    SETTINGS_ACTION_SETTINGS,
    SETTINGS_ACTION_PLAY_PAUSE,
    SETTINGS_ACTION_PAUSE,
    SETTINGS_ACTION_STOP,
    SETTINGS_ACTION_SEEK_BACK,
    SETTINGS_ACTION_SEEK_FORWARD,
    SETTINGS_ACTION_STEP_BACK,
    SETTINGS_ACTION_STEP_FORWARD,
    SETTINGS_ACTION_NEXT_AUDIO,
    SETTINGS_ACTION_PREVIOUS_AUDIO,
    SETTINGS_ACTION_NEXT_SUBTITLE,
    SETTINGS_ACTION_PREVIOUS_SUBTITLE,
    SETTINGS_ACTION_SUBTITLES_TOGGLE,
    SETTINGS_ACTION_AUDIO_DELAY_LATER,
    SETTINGS_ACTION_AUDIO_DELAY_EARLIER,
    SETTINGS_ACTION_SUBTITLE_DELAY_LATER,
    SETTINGS_ACTION_SUBTITLE_DELAY_EARLIER,
    SETTINGS_ACTION_RESET_SUBTITLE_DELAY,
    SETTINGS_ACTION_VOLUME_UP,
    SETTINGS_ACTION_VOLUME_DOWN,
    SETTINGS_ACTION_MUTE,
    SETTINGS_ACTION_SPEED_UP,
    SETTINGS_ACTION_SPEED_DOWN,
    SETTINGS_ACTION_NEXT_CHAPTER,
    SETTINGS_ACTION_PREVIOUS_CHAPTER,
    SETTINGS_ACTION_SCREENSHOT,
    SETTINGS_ACTION_MINIMIZE_WINDOW,
    SETTINGS_ACTION_FULLSCREEN,
    SETTINGS_ACTION_WEB_MODE,
    SETTINGS_ACTION_QUIT,
    SETTINGS_ACTION_CLOSE_WINDOW,
    SETTINGS_ACTION_DEBUG_OVERLAY,
    SETTINGS_ACTION_PICTURE_IN_PICTURE,
    SETTINGS_INPUT_SAVE_FAILED,
    SETTINGS_INPUT_UNBINDABLE_KEY,
    SETTINGS_INPUT_ALSO_BOUND,
    SETTINGS_INPUT_PRESS_NEW_KEY,
    SETTINGS_MEDIA_SYNC_AUDIO_CLOCK,
    SETTINGS_MEDIA_SYNC_DISPLAY_RESAMPLE,
    SETTINGS_MEDIA_SYNC_DISPLAY_ADROP,
    SETTINGS_MEDIA_AUDIO_OUTPUT_BASIC,
    SETTINGS_MEDIA_AUDIO_OUTPUT_SPDIF,
    SETTINGS_MEDIA_AUDIO_OUTPUT_HDMI,
    SETTINGS_MEDIA_LAYOUT_AUTO,
    SETTINGS_MEDIA_LAYOUT_STEREO,
    SETTINGS_MEDIA_LAYOUT_SURROUND51,
    SETTINGS_MEDIA_LAYOUT_SURROUND71,
    SETTINGS_MEDIA_VOLUME_100,
    SETTINGS_MEDIA_VOLUME_150,
    SETTINGS_MEDIA_VOLUME_200,
    SETTINGS_MEDIA_VOLUME_300,
    SETTINGS_MEDIA_DECODE_AUTO,
    SETTINGS_MEDIA_DECODE_OFF,
    SETTINGS_MEDIA_DECODE_D3D11VA,
    SETTINGS_MEDIA_DECODE_NVDEC,
    SETTINGS_MEDIA_DECODE_VAAPI,
    SETTINGS_MEDIA_DECODE_VIDEOTOOLBOX,
    SETTINGS_MEDIA_DECODE_VULKAN,
    SETTINGS_MEDIA_ASPECT_CUSTOM,
    SETTINGS_MEDIA_ASPECT_NORMAL,
    SETTINGS_MEDIA_ASPECT_ZOOM,
    SETTINGS_MEDIA_ASPECT_FORCE4X3,
    SETTINGS_MEDIA_ASPECT_FORCE16X9,
    SETTINGS_MEDIA_ASPECT_FORCE16X9_4X3,
    SETTINGS_MEDIA_ASPECT_STRETCH,
    SETTINGS_MEDIA_ASPECT_NO_SCALING,
    SETTINGS_MEDIA_SYNC_MODE_HEADING,
    SETTINGS_MEDIA_FRAME_DROP,
    SETTINGS_MEDIA_DEFAULT_SPEED,
    SETTINGS_MEDIA_CACHE_READ,
    SETTINGS_MEDIA_CACHE_BACK_BUFFER,
    SETTINGS_MEDIA_CACHE_READ_AHEAD,
    SETTINGS_MEDIA_INTRO_SHOW_BUTTON,
    SETTINGS_MEDIA_INTRO_AUTO,
    SETTINGS_MEDIA_CREDITS_SHOW_BUTTON,
    SETTINGS_MEDIA_CREDITS_AUTO,
    SETTINGS_MEDIA_CREDITS_DELAY,
    SETTINGS_MEDIA_EXTERNAL_PLAYER,
    SETTINGS_MEDIA_BITRATE,
    SETTINGS_MEDIA_CACHE_HELP,
    SETTINGS_MEDIA_AUDIO_OUTPUT_HEADING,
    SETTINGS_MEDIA_AUDIO_CHANNEL_LAYOUT,
    SETTINGS_MEDIA_AUDIO_OUTPUT_DEVICE,
    SETTINGS_MEDIA_SOUND_HEADING,
    SETTINGS_MEDIA_EQ_HEADING,
    SETTINGS_MEDIA_EQ_PRESET,
    SETTINGS_MEDIA_EQ_PRESET_FLAT,
    SETTINGS_MEDIA_EQ_PRESET_BASS,
    SETTINGS_MEDIA_EQ_PRESET_TREBLE,
    SETTINGS_MEDIA_EQ_PRESET_VOICE,
    SETTINGS_MEDIA_EQ_PRESET_CUSTOM,
    SETTINGS_MEDIA_EQ_HELP,
    SETTINGS_MEDIA_LOUDNESS_NORMALIZE,
    SETTINGS_MEDIA_LOUDNESS_TARGET,
    SETTINGS_MEDIA_COMPRESSION,
    SETTINGS_MEDIA_PITCH_CORRECTION,
    SETTINGS_MEDIA_SUBTITLE_ASS_SCALE,
    SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE,
    SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE_DEFAULT,
    SETTINGS_MEDIA_SUBTITLE_ASS_OVERRIDE_HELP,
    SETTINGS_MEDIA_AUDIO_DEVICE_AUTOMATIC,
    SETTINGS_MEDIA_AUDIO_DEVICE_HELP,
    SETTINGS_MEDIA_AUDIO_NORMALIZE,
    SETTINGS_MEDIA_AUDIO_GAPLESS,
    SETTINGS_MEDIA_AUDIO_EXCLUSIVE,
    SETTINGS_MEDIA_AUDIO_MAX_VOLUME,
    SETTINGS_MEDIA_PASSTHROUGH_HEADING,
    SETTINGS_MEDIA_PASSTHROUGH_AC3,
    SETTINGS_MEDIA_PASSTHROUGH_EAC3,
    SETTINGS_MEDIA_PASSTHROUGH_DTS,
    SETTINGS_MEDIA_PASSTHROUGH_DTS_HD,
    SETTINGS_MEDIA_PASSTHROUGH_TRUEHD,
    SETTINGS_MEDIA_SUBTITLE_FONT,
    SETTINGS_MEDIA_SUBTITLE_SIZE,
    SETTINGS_MEDIA_SUBTITLE_OUTLINE_SIZE,
    SETTINGS_MEDIA_SUBTITLE_SECONDARY_POSITION,
    SETTINGS_MEDIA_VIDEO_HARDWARE_DECODE,
    SETTINGS_MEDIA_VIDEO_DEINTERLACE,
    SETTINGS_MEDIA_VIDEO_ASPECT_RATIO,
    SETTINGS_MEDIA_VIDEO_ALLOW_ZOOM,
    SETTINGS_MEDIA_VIDEO_TONE_MAPPING,
    SETTINGS_MEDIA_TONE_MAPPING_AUTO,
    SETTINGS_MEDIA_TONE_MAPPING_BT2390,
    SETTINGS_MEDIA_TONE_MAPPING_MOBIUS,
    SETTINGS_MEDIA_TONE_MAPPING_HABLE,
    SETTINGS_MEDIA_TONE_MAPPING_REINHARD,
    SETTINGS_MEDIA_TONE_MAPPING_CLIP,
    SETTINGS_MEDIA_TONE_MAPPING_HELP,
    SETTINGS_MEDIA_VIDEO_TRANSCODE_ALL,
    SETTINGS_MEDIA_UNIT_MS,
    APP_ERROR_DOWNLOAD_QUEUE,
    APP_PLAYBACK_PROGRESS_STOPPED,
    APP_ERROR_SETTINGS_NOT_SAVED,
    MUSIC_NOUN_TRACK,
    MUSIC_NOUN_TRACKS,
    MUSIC_VERB_WAS,
    MUSIC_VERB_WERE,
    MUSIC_QUEUE_UNAVAILABLE_REMOVED,
    APP_STATUS_SIGNING_IN,
    BROWSE_SORT_NAME,
    BROWSE_SORT_RELEASE_DATE,
    BROWSE_SORT_RATING,
    BROWSE_PLAYED_ANY,
    BROWSE_PLAYED_PLAYED,
    BROWSE_PLAYED_UNPLAYED,
    SHELL_NAV_ALL_ITEMS,
    SHELL_NAV_MOVIES,
    SHELL_NAV_SERIES,
    SHELL_NAV_MUSIC,
    SHELL_NAV_PHOTOS,
    SHELL_NAV_PLAYLISTS,
    SHELL_NAV_DOWNLOADS,
    SHELL_NAV_LIVE_TV,
    SHELL_NAV_DASHBOARD,
    SHELL_NAV_SETTINGS,
    SHELL_RAIL_HOME,
    SHELL_RAIL_MOVIES,
    SHELL_RAIL_SERIES,
    SHELL_RAIL_MUSIC,
    SHELL_RAIL_PHOTOS,
    SHELL_RAIL_PLAYLISTS,
    SHELL_RAIL_DOWNLOADS,
    SHELL_RAIL_LIVE_TV,
    SHELL_RAIL_DASHBOARD,
    SHELL_RAIL_SETTINGS,
    APP_ERROR_WORKER_START,
    MUSIC_QUEUE_INVALID_MOVED,
    MUSIC_QUEUE_READ_FAILED,
    PROFILE_NOTICE_TITLE,
    APP_BUTTON_OK,
    PROFILE_ERROR_UNAVAILABLE,
    CONNECT_ERROR_SERVER_REQUIRED,
    CONNECT_ERROR_USERNAME_REQUIRED,
    APP_ERROR_QUEUE_NETWORK,
    CONNECT_ERROR_CHECK_ADDRESS,
    CONNECT_ERROR_QUICK_CONNECT_DISABLED,
    CONNECT_STATUS_STARTING_QUICK_CONNECT,
    LIVE_TV_NOTICE_UNAVAILABLE,
    LIVE_TV_STATUS_TUNING,
    LIVE_TV_ERROR_QUEUE_TUNE,
    LIVE_TV_ERROR_TUNE_FAILED,
    LIVE_TV_NOTICE_STOPPED,
    MUSIC_QUEUE_FINISHED,
    MUSIC_STATUS_PREPARING_NEXT,
    MUSIC_STATUS_PREPARING_PREVIOUS,
    MUSIC_QUEUE_REFRESH_FAILED,
    MUSIC_QUEUE_NO_SAVED_TRACKS,
    MUSIC_QUEUE_RESTORED,
    MUSIC_QUEUE_NOTHING_TO_RESUME,
    MUSIC_QUEUE_SAVE_FAILED,
    MUSIC_QUEUE_PERSISTENCE_UNAVAILABLE,
    PLAYLISTS_NOTICE_NO_AUDIO,
    PLAYLISTS_NOTICE_TOO_LARGE,
    PLAYLISTS_STATUS_PREPARING,
    DOWNLOADS_ERROR_NO_PROFILE,
    PLAYBACK_ERROR_NO_COMPATIBLE_SOURCE,
    PLAYBACK_STATUS_PREPARING_LOCAL,
    PLAYBACK_STATUS_OPENING_LOCAL,
    APP_ERROR_NETWORK_WORKER_UNAVAILABLE,
    MUSIC_QUEUE_REFRESH_RECONNECT,
    PLAYLISTS_NOTICE_CREATED,
    PLAYLISTS_ERROR_CREATE_FAILED,
    PLAYLISTS_NOTICE_ADDED,
    PLAYLISTS_NOTICE_REMOVED,
    PLAYLISTS_NOTICE_MOVED,
    PLAYLISTS_ERROR_ADD_FAILED,
    PLAYLISTS_ERROR_REMOVE_FAILED,
    PLAYLISTS_ERROR_REORDER_FAILED,
    DETAIL_ERROR_WATCHED_FAILED,
    DETAIL_ERROR_FAVORITE_FAILED,
    PLAYBACK_ERROR_SESSION_UPDATE,
];

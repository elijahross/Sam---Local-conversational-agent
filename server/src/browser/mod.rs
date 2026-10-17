//! Browser handling: host-Chrome "screens" over CDP, consent walls, search, page extraction,
//! and the research pipeline. Navigation decisions use Laya + DOM heuristics (no vision LLM);
//! page text reaches a model only as capped, cleaned excerpts — never raw HTML.
pub mod agent;
pub mod cdp;
pub mod chat;
pub mod consent;
pub mod extract;
pub mod gmail;
pub mod linkedin;
pub mod login;
pub mod listings;
pub mod park;
pub mod reddit;
pub mod research;
pub mod scrape;
pub mod search;
pub mod workspace;

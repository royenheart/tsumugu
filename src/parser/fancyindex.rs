// Nginx fancyindex parser

use crate::{
    listing::{FileSize, FileType, ListItem},
    utils::get,
};

use super::*;
use anyhow::Result;
use chrono::NaiveDateTime;
use scraper::{Html, Selector};

#[derive(Debug, Clone, Default)]
pub struct FancyIndexListingParser;

impl Parser for FancyIndexListingParser {
    fn get_list(&self, client: &Client, url: &Url) -> Result<ListResult> {
        let resp = get(client, url.clone())?;
        let url = resp.url().clone();
        let body = resp.text()?;
        assert_if_url_has_no_trailing_slash(&url);
        let document = Html::parse_document(&body);
        let selector = Selector::parse("tbody tr").unwrap();
        let mut items = Vec::new();
        for element in document.select(&selector) {
            let link_selector = Selector::parse("td.link a").unwrap();
            let size_selector = Selector::parse("td.size").unwrap();
            let date_selector = Selector::parse("td.date").unwrap();

            let a = element.select(&link_selector).next().unwrap();
            let href = a.value().attr("href").unwrap();
            let displayed_filename = a.inner_html();

            if displayed_filename == "Parent Directory/" || href == "../" {
                continue;
            }

            let name = get_real_name_from_href(href);
            let href = url.join(href)?;
            let type_ = if href.as_str().ends_with('/') {
                FileType::Directory
            } else {
                FileType::File
            };
            let size = element.select(&size_selector).next().unwrap().inner_html();
            let size = size.trim();
            let date = element.select(&date_selector).next().unwrap().inner_html();
            let date = date.trim();

            // decide which time format to use
            let date_fmt = if date.len() == 16 {
                "%Y-%m-%d %H:%M"
            } else if date.len() == 19 {
                "%Y-%m-%d %H:%M:%S"
            } else {
                unreachable!()
            };
            let date = NaiveDateTime::parse_from_str(date, date_fmt)?;

            items.push(ListItem::new(
                href,
                name,
                type_,
                {
                    if size == "-" {
                        None
                    } else {
                        let (n_size, unit) = FileSize::get_humanized(size);
                        Some(FileSize::HumanizedBinary(n_size, unit))
                    }
                },
                date,
            ));
        }

        Ok(ListResult::List(items))
    }
}

#[cfg(test)]
mod tests {
    use crate::listing::SizeUnit;
    use super::*;

    #[test]
    fn test_njumirrors() {
        let client = reqwest::blocking::Client::new();
        let items = FancyIndexListingParser.get_list(
            &client,
            &Url::parse("http://localhost:1921/bmclapi/").unwrap(),
        ).unwrap();
        match items {
            ListResult::List(items) => {
                assert_eq!(items[0].name, "bouncycastle");
                assert_eq!(items[0].type_, FileType::Directory);
                assert_eq!(items[0].size, None);
                assert_eq!(items[0].mtime, NaiveDateTime::parse_from_str("2024-04-23 19:01:54", "%Y-%m-%d %H:%M:%S").unwrap());
                assert_eq!(items[items.len() - 1].name, "lwjgURL");
                assert_eq!(items[items.len() - 1].type_, FileType::File);
                assert_eq!(items[items.len() - 1].size, Some(FileSize::HumanizedBinary(1767.0, SizeUnit::B)));
                assert_eq!(items[items.len() - 1].mtime, NaiveDateTime::parse_from_str("2021-04-30 20:55:32", "%Y-%m-%d %H:%M:%S").unwrap());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_loongnix() {
        let client = reqwest::blocking::Client::new();
        let items = FancyIndexListingParser.get_list(
            &client,
            &Url::parse("http://localhost:1921/loongnix/").unwrap(),
        ).unwrap();
        match items {
            ListResult::List(items) => {
                assert_eq!(items[0].name, "contrib");
                assert_eq!(items[0].type_, FileType::Directory);
                assert_eq!(items[0].size, None);
                assert_eq!(items[0].mtime, NaiveDateTime::parse_from_str("2023-08-15 05:48", "%Y-%m-%d %H:%M").unwrap());
                assert_eq!(items[items.len() - 1].name, "Release.gpg");
                assert_eq!(items[items.len() - 1].type_, FileType::File);
                assert_eq!(items[items.len() - 1].size, Some(FileSize::HumanizedBinary(659.0, SizeUnit::B)));
                assert_eq!(items[items.len() - 1].mtime, NaiveDateTime::parse_from_str("2023-08-15 05:48", "%Y-%m-%d %H:%M").unwrap());
            }
            _ => unreachable!(),
        }
    }
}
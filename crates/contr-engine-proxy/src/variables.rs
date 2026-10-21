use contr_core::proxy::Proxy;

/// The environment variables that hand a proxy to programs, each set in upper and lower case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProxyVariables {
    /// Upper-case names with their values.
    values: Vec<(&'static str, String)>,
}

impl ProxyVariables {
    /// Variables of `proxy`, or `None` when it reaches every destination directly.
    pub(crate) fn of(proxy: &Proxy) -> Option<Self> {
        if proxy.is_direct() {
            return None;
        }
        let mut values = Vec::new();
        if let Some(http) = &proxy.http {
            values.push(("HTTP_PROXY", http.clone()));
        }
        if let Some(https) = &proxy.https {
            values.push(("HTTPS_PROXY", https.clone()));
        }
        if !proxy.exclude.is_empty() {
            values.push(("NO_PROXY", proxy.exclude.join(",")));
        }

        Some(Self { values })
    }

    /// Names and values to add where `defined` names are set: both cases of every variable that `defined` sets in
    /// neither case.
    pub(crate) fn missing(&self, defined: &[&str]) -> Vec<(String, &str)> {
        self.values
            .iter()
            .filter(|(name, _)| !defined.iter().any(|defined| defined.eq_ignore_ascii_case(name)))
            .flat_map(|(name, value)| {
                [
                    ((*name).to_owned(), value.as_str()),
                    (name.to_ascii_lowercase(), value.as_str()),
                ]
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(http: Option<&str>, https: Option<&str>) -> Proxy {
        Proxy {
            http: http.map(str::to_owned),
            https: https.map(str::to_owned),
            exclude: vec!["localhost".to_owned(), ".corp".to_owned()],
        }
    }

    #[test]
    fn a_direct_proxy_has_no_variables() {
        assert_eq!(ProxyVariables::of(&proxy(None, None)), None);
    }

    #[test]
    fn sets_both_cases_of_what_is_missing() {
        let variables = ProxyVariables::of(&proxy(Some("http://p:3128"), None)).unwrap();

        assert_eq!(
            variables.missing(&["PATH", "no_proxy"]),
            [
                ("HTTP_PROXY".to_owned(), "http://p:3128"),
                ("http_proxy".to_owned(), "http://p:3128"),
            ]
        );
        assert!(variables.missing(&["Http_Proxy", "NO_PROXY"]).is_empty());
        let all: Vec<_> = variables.missing(&[]).into_iter().map(|(name, _)| name).collect();
        assert_eq!(all, ["HTTP_PROXY", "http_proxy", "NO_PROXY", "no_proxy"]);
    }
}

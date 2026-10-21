//! Solve requests of BuildKit's gRPC API, which carry build arguments as frontend options.

use crate::variables::ProxyVariables;

/// Frontend option prefix the Dockerfile frontend reads build arguments from.
const BUILD_ARG: &str = "build-arg:";

/// Field numbers of the frontend name and the frontend options of a Solve request in BuildKit's API, which BuildKit
/// never renumbers.
#[derive(Clone, Copy, Debug)]
pub(super) struct SolveFields {
    frontend: u64,
    options: u64,
}

/// `moby.buildkit.v1.frontend.LLBBridge/Solve`, which Buildx calls for every build.
const GATEWAY_SOLVE: SolveFields = SolveFields {
    frontend: 2,
    options: 3,
};

/// `moby.buildkit.v1.Control/Solve`, which clients without a gateway call.
const CONTROL_SOLVE: SolveFields = SolveFields {
    frontend: 6,
    options: 7,
};

/// Fields of the Solve request a call to `path` carries, or `None` for any other method.
pub(super) fn solve_fields(path: &str) -> Option<SolveFields> {
    match path {
        "/moby.buildkit.v1.frontend.LLBBridge/Solve" => Some(GATEWAY_SOLVE),
        "/moby.buildkit.v1.Control/Solve" => Some(CONTROL_SOLVE),
        _ => None,
    }
}

/// The gRPC request body `call` with the missing build arguments added, or `None` when it is not one uncompressed
/// message, names no frontend, or lacks nothing.
pub(super) fn solve_call(call: &[u8], fields: SolveFields, variables: &ProxyVariables) -> Option<Vec<u8>> {
    let (&[compressed, a, b, c, d], message) = call.split_first_chunk::<5>()?;
    let length = usize::try_from(u32::from_be_bytes([a, b, c, d])).ok()?;
    if compressed != 0 || message.len() != length {
        return None;
    }
    let added = build_args(message, fields, variables)?;

    let mut rewritten = Vec::with_capacity(call.len() + added.len());
    rewritten.push(0);
    rewritten.extend_from_slice(&u32::try_from(message.len() + added.len()).ok()?.to_be_bytes());
    rewritten.extend_from_slice(message);
    rewritten.extend_from_slice(&added);
    Some(rewritten)
}

/// Encoded frontend option entries for the proxy variables the Solve request `message` does not set; appending them
/// adds them to its options map.
fn build_args(message: &[u8], fields: SolveFields, variables: &ProxyVariables) -> Option<Vec<u8>> {
    let mut names_frontend = false;
    let mut defined = Vec::new();
    for (number, value) in wire::length_delimited(message)? {
        if number == fields.frontend {
            names_frontend = !value.is_empty();
        } else if number == fields.options
            && let Ok(key) = std::str::from_utf8(wire::map_key(value)?)
            && let Some(name) = key.strip_prefix(BUILD_ARG)
        {
            defined.push(name);
        }
    }
    if !names_frontend {
        return None;
    }

    let mut added = Vec::new();
    for (name, value) in variables.missing(&defined) {
        let mut entry = Vec::new();
        wire::put_bytes(&mut entry, 1, format!("{BUILD_ARG}{name}").as_bytes());
        wire::put_bytes(&mut entry, 2, value.as_bytes());
        wire::put_bytes(&mut added, fields.options, &entry);
    }
    (!added.is_empty()).then_some(added)
}

/// The parts of the protobuf wire format a Solve request needs.
mod wire {
    const VARINT: u64 = 0;
    const FIXED64: u64 = 1;
    const LENGTH_DELIMITED: u64 = 2;
    const FIXED32: u64 = 5;

    fn varint(input: &mut &[u8]) -> Option<u64> {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = input.split_first()?;
            *input = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    /// Field numbers and contents of the length-delimited fields of `message`, or `None` when it is malformed or
    /// holds a group.
    pub(super) fn length_delimited(message: &[u8]) -> Option<Vec<(u64, &[u8])>> {
        let mut input = message;
        let mut fields = Vec::new();
        while !input.is_empty() {
            let key = varint(&mut input)?;
            match key & 7 {
                VARINT => {
                    varint(&mut input)?;
                }
                FIXED64 => input = input.get(8..)?,
                LENGTH_DELIMITED => {
                    let length = usize::try_from(varint(&mut input)?).ok()?;
                    let (value, rest) = input.split_at_checked(length)?;
                    fields.push((key >> 3, value));
                    input = rest;
                }
                FIXED32 => input = input.get(4..)?,
                _ => return None,
            }
        }
        Some(fields)
    }

    /// The key of a map entry.
    pub(super) fn map_key(entry: &[u8]) -> Option<&[u8]> {
        let key = length_delimited(entry)?
            .into_iter()
            .rfind(|(number, _)| *number == 1)
            .map_or(&[][..], |(_, key)| key);
        Some(key)
    }

    fn put_varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push(value.to_le_bytes()[0] | 0x80);
            value >>= 7;
        }
        out.push(value.to_le_bytes()[0]);
    }

    pub(super) fn put_bytes(out: &mut Vec<u8>, number: u64, value: &[u8]) {
        put_varint(out, (number << 3) | LENGTH_DELIMITED);
        put_varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }
}

#[cfg(test)]
mod tests {
    use contr_core::proxy::Proxy;

    use super::*;

    fn variables() -> ProxyVariables {
        ProxyVariables::of(&Proxy {
            http: Some("http://198.19.255.254:3128".to_owned()),
            https: None,
            exclude: vec!["localhost".to_owned()],
        })
        .unwrap()
    }

    fn entry(key: &str, value: &str) -> Vec<u8> {
        let mut entry = Vec::new();
        wire::put_bytes(&mut entry, 1, key.as_bytes());
        wire::put_bytes(&mut entry, 2, value.as_bytes());
        entry
    }

    /// A gateway Solve request: a definition, a frontend, `options`, and a varint field after them.
    fn gateway_solve(frontend: &str, options: &[(&str, &str)]) -> Vec<u8> {
        let mut message = Vec::new();
        wire::put_bytes(&mut message, 1, b"\x0a\x02ab");
        wire::put_bytes(&mut message, 2, frontend.as_bytes());
        for (key, value) in options {
            wire::put_bytes(&mut message, 3, &entry(key, value));
        }
        message.extend_from_slice(&[0x50, 0x01]);
        message
    }

    fn framed(message: &[u8]) -> Vec<u8> {
        let mut call = vec![0];
        call.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
        call.extend_from_slice(message);
        call
    }

    /// Frontend options of a framed Solve call, in wire order.
    fn options(call: &[u8], fields: SolveFields) -> Vec<(String, String)> {
        let message = &call[5..];
        assert_eq!(
            usize::try_from(u32::from_be_bytes(call[1..5].try_into().unwrap())).unwrap(),
            message.len()
        );
        wire::length_delimited(message)
            .unwrap()
            .into_iter()
            .filter(|(number, _)| *number == fields.options)
            .map(|(_, entry)| {
                let fields = wire::length_delimited(entry).unwrap();
                let text = |number| String::from_utf8(fields.iter().find(|(n, _)| *n == number).unwrap().1.to_vec());
                (text(1).unwrap(), text(2).unwrap())
            })
            .collect()
    }

    #[test]
    fn adds_build_arguments_to_gateway_solves() {
        let call = framed(&gateway_solve(
            "dockerfile.v0",
            &[("build-arg:no_proxy", "mine"), ("target", "app")],
        ));

        let rewritten = solve_call(&call, GATEWAY_SOLVE, &variables()).unwrap();

        assert_eq!(
            options(&rewritten, GATEWAY_SOLVE),
            [
                ("build-arg:no_proxy".to_owned(), "mine".to_owned()),
                ("target".to_owned(), "app".to_owned()),
                (
                    "build-arg:HTTP_PROXY".to_owned(),
                    "http://198.19.255.254:3128".to_owned()
                ),
                (
                    "build-arg:http_proxy".to_owned(),
                    "http://198.19.255.254:3128".to_owned()
                ),
            ]
        );
        assert!(rewritten[5..].starts_with(&gateway_solve("dockerfile.v0", &[])[..6]));
    }

    #[test]
    fn adds_build_arguments_to_control_solves() {
        let mut message = Vec::new();
        wire::put_bytes(&mut message, 1, b"ref");
        wire::put_bytes(&mut message, 6, b"dockerfile.v0");

        let rewritten = solve_call(&framed(&message), CONTROL_SOLVE, &variables()).unwrap();

        assert_eq!(options(&rewritten, CONTROL_SOLVE).len(), 4);
    }

    #[test]
    fn leaves_other_calls_unchanged() {
        let complete = gateway_solve(
            "dockerfile.v0",
            &[("build-arg:HTTP_PROXY", "a"), ("build-arg:NO_PROXY", "b")],
        );
        assert_eq!(solve_call(&framed(&complete), GATEWAY_SOLVE, &variables()), None);

        let definition_only = gateway_solve("", &[]);
        assert_eq!(solve_call(&framed(&definition_only), GATEWAY_SOLVE, &variables()), None);

        let mut compressed = framed(&gateway_solve("dockerfile.v0", &[]));
        compressed[0] = 1;
        assert_eq!(solve_call(&compressed, GATEWAY_SOLVE, &variables()), None);

        let mut two_messages = framed(&gateway_solve("dockerfile.v0", &[]));
        two_messages.extend(framed(b""));
        assert_eq!(solve_call(&two_messages, GATEWAY_SOLVE, &variables()), None);

        let truncated = framed(&[0x12, 0x05, b'a']);
        assert_eq!(solve_call(&truncated, GATEWAY_SOLVE, &variables()), None);
        assert_eq!(solve_call(&[0, 0], GATEWAY_SOLVE, &variables()), None);
    }

    #[test]
    fn encodes_long_varints() {
        let mut out = Vec::new();
        wire::put_bytes(&mut out, 3, &[7; 300]);

        assert_eq!(&out[..3], [0x1a, 0xac, 0x02]);
        assert_eq!(wire::length_delimited(&out).unwrap(), [(3, &[7; 300][..])]);
    }
}

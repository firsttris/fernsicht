//! UPnP IGD: asking the router to forward a port, the way most home
//! routers (a FritzBox among them) still understand. Pure: the SSDP
//! question, reading the router's description, the SOAP requests and
//! answers (as miniupnpc's upnpcommands.c and minisoap.c send them:
//! HTTP/1.0, one write, no chunks).

/// Where SSDP questions go (IPv4 multicast).
pub const SSDP: &str = "239.255.255.250:1900";

/// The services that forward ports, best first.
const SERVICES: [&str; 4] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:2",
];

/// "Which internet gateways are there?"
pub fn search_request() -> String {
    "M-SEARCH * HTTP/1.1\r\n\
     HOST: 239.255.255.250:1900\r\n\
     MAN: \"ssdp:discover\"\r\n\
     MX: 2\r\n\
     ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
        .to_owned()
}

/// A header's value in an HTTP-like message (case-insensitive name).
fn header<'a>(msg: &'a str, name: &str) -> Option<&'a str> {
    msg.lines().find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// The description's address from an SSDP answer (its LOCATION).
pub fn location(answer: &str) -> Option<&str> {
    header(answer, "LOCATION").filter(|l| l.starts_with("http://"))
}

/// The text of the first `<tag>…</tag>` in `xml` (any namespace prefix).
fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut rest = xml;
    loop {
        let open = rest.find('<')?;
        rest = &rest[open + 1..];
        let end = rest.find('>')?;
        let name = rest[..end].split_whitespace().next()?.trim_end_matches('/');
        let local = name.rsplit(':').next()?;
        if local == tag && !rest[..end].ends_with('/') {
            let body = &rest[end + 1..];
            let close = body.find("</")?;
            return Some(body[..close].trim());
        }
        rest = &rest[end + 1..];
    }
}

/// `scheme://host:port` of a URL.
fn origin(url: &str) -> Option<&str> {
    let after = url.find("://")? + 3;
    let end = url[after..].find('/').map_or(url.len(), |i| after + i);
    Some(&url[..end])
}

/// The port-forwarding service in a router's description at `location`:
/// its type and its control URL (absolute).
pub fn control(description: &str, location: &str) -> Option<(String, String)> {
    let base = element(description, "URLBase")
        .and_then(origin)
        .or_else(|| origin(location))?;
    let services: Vec<(&str, &str)> = description
        .split("<service>")
        .skip(1)
        .filter_map(|s| Some((element(s, "serviceType")?, element(s, "controlURL")?)))
        .collect();
    SERVICES.iter().find_map(|want| {
        let (ty, url) = services.iter().find(|(ty, _)| ty == want)?;
        let url = if url.starts_with("http://") {
            (*url).to_owned()
        } else {
            format!("{base}/{}", url.trim_start_matches('/'))
        };
        Some(((*ty).to_owned(), url))
    })
}

/// A SOAP request to `control` (the service's URL), as one HTTP/1.0
/// message.
pub fn soap(control: &str, service: &str, action: &str, args: &[(&str, &str)]) -> Option<String> {
    let base = origin(control)?;
    let host = base.split_once("://")?.1;
    let path = &control[base.len()..];
    let path = if path.is_empty() { "/" } else { path };
    let args: String = args
        .iter()
        .map(|(k, v)| format!("<{k}>{v}</{k}>"))
        .collect();
    let body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action} xmlns:u=\"{service}\">{args}</u:{action}></s:Body></s:Envelope>\r\n"
    );
    Some(format!(
        "POST {path} HTTP/1.0\r\nHost: {host}\r\nContent-Length: {}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{service}#{action}\"\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    ))
}

/// Forward UDP `port` to `client` (the host's LAN address) for
/// `lifetime` seconds.
pub fn add_port_mapping(
    control: &str,
    service: &str,
    client: &str,
    port: u16,
    lifetime: u32,
) -> Option<String> {
    let port = port.to_string();
    let lifetime = lifetime.to_string();
    soap(
        control,
        service,
        "AddPortMapping",
        &[
            ("NewRemoteHost", ""),
            ("NewExternalPort", &port),
            ("NewProtocol", "UDP"),
            ("NewInternalPort", &port),
            ("NewInternalClient", client),
            ("NewEnabled", "1"),
            ("NewPortMappingDescription", "Fernsicht"),
            ("NewLeaseDuration", &lifetime),
        ],
    )
}

pub fn delete_port_mapping(control: &str, service: &str, port: u16) -> Option<String> {
    let port = port.to_string();
    soap(
        control,
        service,
        "DeletePortMapping",
        &[
            ("NewRemoteHost", ""),
            ("NewExternalPort", &port),
            ("NewProtocol", "UDP"),
        ],
    )
}

pub fn get_external_ip(control: &str, service: &str) -> Option<String> {
    soap(control, service, "GetExternalIPAddress", &[])
}

/// What a SOAP answer says: success with its body, or the UPnP error code
/// (e.g. 718: the port is mapped to someone else; 725: only permanent
/// leases).
#[derive(Debug, PartialEq, Eq)]
pub enum Reply<'a> {
    Ok(&'a str),
    Fault(u16),
    Malformed,
}

pub fn reply(http: &str) -> Reply<'_> {
    let status = http
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok());
    let body = http.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    match status {
        Some(200) => Reply::Ok(body),
        Some(_) => element(body, "errorCode")
            .and_then(|c| c.parse().ok())
            .map_or(Reply::Malformed, Reply::Fault),
        None => Reply::Malformed,
    }
}

/// The public address in a GetExternalIPAddress answer.
pub fn external_ip(body: &str) -> Option<std::net::IpAddr> {
    element(body, "NewExternalIPAddress")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A FritzBox's description, shortened to what matters.
    const FRITZBOX: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
<friendlyName>FRITZ!Box 7590</friendlyName>
<serviceList><service><serviceType>urn:schemas-any-com:service:Any:1</serviceType>
<controlURL>/igdupnp/control/any</controlURL></service></serviceList>
<deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
<serviceList><service>
<serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType>
<serviceId>urn:upnp-org:serviceId:WANCommonIFC1</serviceId>
<controlURL>/igdupnp/control/WANCommonIFC1</controlURL>
</service></serviceList>
<deviceList><device><deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
<serviceList><service>
<serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
<serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
<controlURL>/igdupnp/control/WANIPConn1</controlURL>
<eventSubURL>/igdupnp/control/WANIPConn1</eventSubURL>
<SCPDURL>/igdconnSCPD.xml</SCPDURL>
</service></serviceList></device></deviceList></device></deviceList>
<presentationURL>http://fritz.box</presentationURL></device></root>"#;

    #[test]
    fn the_router_is_found_and_its_service_read() {
        assert!(
            search_request().contains("ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1")
        );
        let answer = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\n\
                      Location: http://192.168.178.1:49000/igddesc.xml\r\nST: upnp:rootdevice\r\n\r\n";
        let loc = location(answer).unwrap();
        assert_eq!(loc, "http://192.168.178.1:49000/igddesc.xml");
        assert_eq!(
            control(FRITZBOX, loc),
            Some((
                "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
                "http://192.168.178.1:49000/igdupnp/control/WANIPConn1".into()
            ))
        );
        // A PPP router with a URLBase.
        let ppp = "<root><URLBase>http://10.0.0.1:5000/</URLBase><service>\
                   <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>\
                   <controlURL>upnp/control/WANPPPConn1</controlURL></service></root>";
        assert_eq!(
            control(ppp, "http://10.0.0.1:80/desc.xml").unwrap().1,
            "http://10.0.0.1:5000/upnp/control/WANPPPConn1"
        );
        assert_eq!(control("<root></root>", loc), None);
        assert_eq!(location("HTTP/1.1 200 OK\r\n\r\n"), None);
    }

    #[test]
    fn soap_requests_are_one_http_message() {
        let s = "urn:schemas-upnp-org:service:WANIPConnection:1";
        let r = add_port_mapping(
            "http://192.168.178.1:49000/igdupnp/control/WANIPConn1",
            s,
            "192.168.178.20",
            47800,
            3600,
        )
        .unwrap();
        assert!(r.starts_with(
            "POST /igdupnp/control/WANIPConn1 HTTP/1.0\r\nHost: 192.168.178.1:49000\r\n"
        ));
        assert!(r.contains(&format!("SOAPAction: \"{s}#AddPortMapping\"")));
        assert!(
            r.contains("<NewExternalPort>47800</NewExternalPort><NewProtocol>UDP</NewProtocol>")
        );
        assert!(r.contains("<NewInternalClient>192.168.178.20</NewInternalClient>"));
        let (head, body) = r.split_once("\r\n\r\n").unwrap();
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        assert!(
            delete_port_mapping("http://h/c", s, 1)
                .unwrap()
                .contains("#DeletePortMapping")
        );
        assert!(
            get_external_ip("http://h:1/c", s)
                .unwrap()
                .contains("<u:GetExternalIPAddress xmlns:u")
        );
    }

    #[test]
    fn answers_and_faults_are_read() {
        // miniupnpc's testreplyparse/GetExternalIPAddress.xml.
        let ok = "HTTP/1.0 200 OK\r\nContent-Type: text/xml\r\n\r\n<?xml version=\"1.0\"?><s:Envelope \
                  xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
                  <s:Body><u:GetExternalIPAddressResponse xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">\
                  <NewExternalIPAddress>1.2.3.4</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>";
        let Reply::Ok(body) = reply(ok) else { panic!() };
        assert_eq!(external_ip(body), Some("1.2.3.4".parse().unwrap()));
        let fault = "HTTP/1.0 500 Internal Server Error\r\n\r\n<s:Envelope><s:Body><s:Fault>\
                     <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>718</errorCode>\
                     <errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail>\
                     </s:Fault></s:Body></s:Envelope>";
        assert_eq!(reply(fault), Reply::Fault(718));
        assert_eq!(reply("garbage"), Reply::Malformed);
        assert_eq!(reply("HTTP/1.0 500 x\r\n\r\nno xml"), Reply::Malformed);
    }
}

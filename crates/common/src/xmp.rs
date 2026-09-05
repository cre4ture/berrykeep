//! XMP sidecar metadata for media objects.
//!
//! Ironmesh keeps user labels in an XMP sidecar object next to the media object
//! (`album/photo.jpg` -> `album/photo.jpg.xmp`) instead of writing them into the
//! media file. Any byte change to the media file would produce new chunks, a new
//! manifest and a new object version, which is exactly what labelling must not
//! cost.
//!
//! Sidecars are regularly authored by third-party tools (Lightroom, digiKam,
//! darktable). Those tools store a large number of namespaces Ironmesh does not
//! model, so parsing has to be lossless: the raw XML event stream of the packet
//! is retained verbatim and only the `dc:subject` property is regenerated when
//! the packet is written back.
//!
//! The module is split into three layers:
//! * reading raw events from bytes (`read_resolved_events`),
//! * pure analysis of that event stream (`scan_structure`, `plan_layout`,
//!   `read_keywords`),
//! * pure generation of the `dc:subject` markup (`XmpSidecar::render_property`).

use anyhow::{Context, Result, bail};
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesCData, BytesEnd, BytesRef, BytesStart, BytesText, Event};
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, Writer};
use std::ops::Range;

/// File name suffix of an XMP sidecar object.
///
/// The full media extension is retained (`photo.jpg` -> `photo.jpg.xmp`) so that
/// `photo.jpg` and `photo.png` in the same directory cannot share a sidecar.
pub const XMP_SIDECAR_SUFFIX: &str = ".xmp";

const RDF_NAMESPACE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const DC_NAMESPACE: &str = "http://purl.org/dc/elements/1.1/";
const EXIF_NAMESPACE: &str = "http://ns.adobe.com/exif/1.0/";
const BERRYKEEP_NAMESPACE: &str = "https://berrykeep.app/ns/1.0/";
const RDF_PREFIX_FALLBACK: &str = "rdf";
const DC_PREFIX_FALLBACK: &str = "dc";
const EXIF_PREFIX_FALLBACK: &str = "exif";
const BERRYKEEP_PREFIX_FALLBACK: &str = "berrykeep";
const INDENT_STEP_FALLBACK: &str = " ";

/// Minimal, valid and empty XMP packet used by [`XmpSidecar::new_empty`].
const EMPTY_PACKET: &str = concat!(
    "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
    "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"ironmesh\">\n",
    " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
    "  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"/>\n",
    " </rdf:RDF>\n",
    "</x:xmpmeta>\n",
    "<?xpacket end=\"w\"?>\n",
);

/// An XMP packet that can be inspected and edited without losing unknown content.
///
/// Everything except the `dc:subject` property is kept as the original XML event
/// stream, so [`XmpSidecar::to_bytes`] reproduces third-party markup byte for
/// byte, including formatting, comments and namespaces Ironmesh knows nothing
/// about.
#[derive(Debug, Clone)]
pub struct XmpSidecar {
    /// Every event of the source packet except the `dc:subject` property.
    events: Vec<ResolvedEvent>,
    /// Typed view of `dc:subject`, regenerated on write.
    keywords: Vec<String>,
    /// Where the regenerated property is spliced back into `events`.
    placement: Option<SubjectPlacement>,
    /// Whitespace layout of the packet, reused for generated markup.
    indent: Option<Indentation>,
    /// Prefixes and declarations used for generated markup.
    namespaces: GeneratedNamespaces,
    /// A newly requested geolocation write. Existing GPS properties remain in
    /// the lossless event stream; callers only set this after checking that GPS
    /// is absent, so a write never creates competing locations.
    geo_inference: Option<XmpGeoInference>,
}

/// BerryKeep's auditable result of a confirmed geolocation inference.
#[derive(Debug, Clone, PartialEq)]
pub struct XmpGeoInference {
    pub latitude: f64,
    pub longitude: f64,
    pub method: String,
    pub run_id: String,
    pub confidence: String,
    pub reference_distance_seconds: Option<u64>,
    pub previous_anchor_distance_seconds: Option<u64>,
    pub next_anchor_distance_seconds: Option<u64>,
    pub estimated_speed_kmh: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct XmpGeoLocation {
    pub latitude: f64,
    pub longitude: f64,
    /// `true` when the sidecar marks this location as a BerryKeep inference.
    /// Inference runs exclude such locations as anchors so derived positions
    /// can never become new ground-truth input.
    pub inferred_by_berrykeep: bool,
}

impl XmpSidecar {
    /// Parses an existing XMP packet, preserving all unknown elements and
    /// namespaces.
    ///
    /// Fails when the bytes are not well-formed XML or do not contain an
    /// `rdf:RDF` element, which every XMP packet is required to have.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut events = read_resolved_events(bytes)?;
        let structure = scan_structure(&events);
        if structure.rdf_root.is_none() {
            bail!("not an XMP packet: no rdf:RDF element found");
        }

        let keywords = match structure.subject {
            Some(subject) => read_keywords(&events, subject)?,
            None => Vec::new(),
        };
        let namespaces = generated_namespaces(&events, &structure);
        let layout = plan_layout(&events, &structure);
        if let Some(replaced) = layout.replaced.clone() {
            events.drain(replaced);
        }

        Ok(Self {
            events,
            keywords,
            placement: layout.placement,
            indent: layout.indent,
            namespaces,
            geo_inference: None,
        })
    }

    /// Creates a minimal, valid and empty XMP packet.
    pub fn new_empty() -> Self {
        Self::parse(EMPTY_PACKET.as_bytes())
            .expect("the built-in empty XMP packet must always be parsable")
    }

    /// Keywords stored in `dc:subject`, the IPTC "Keywords" equivalent.
    pub fn keywords(&self) -> &[String] {
        &self.keywords
    }

    /// Replaces the `dc:subject` keywords, leaving every other property
    /// untouched.
    ///
    /// An empty list removes the property from the packet entirely.
    pub fn set_keywords(&mut self, keywords: Vec<String>) {
        self.keywords = keywords;
    }

    /// Returns a valid location already present in the XMP packet. Both common
    /// XMP attribute and element property forms are accepted.
    pub fn geo_location(&self) -> Option<XmpGeoLocation> {
        read_geo_location(&self.events)
    }

    /// Adds canonical EXIF GPS fields and BerryKeep provenance on a fresh
    /// `rdf:Description`. Unknown XMP content is retained verbatim.
    pub fn set_geo_inference(&mut self, inference: XmpGeoInference) -> Result<()> {
        if !inference.latitude.is_finite()
            || !(-90.0..=90.0).contains(&inference.latitude)
            || !inference.longitude.is_finite()
            || !(-180.0..=180.0).contains(&inference.longitude)
        {
            bail!("invalid XMP geolocation coordinates");
        }
        self.geo_inference = Some(inference);
        Ok(())
    }

    /// Serializes the packet back to bytes.
    ///
    /// Fails when keywords are set but the packet offers no place to store them,
    /// which is the case for a packet whose `rdf:RDF` element is self-closing.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut writer = Writer::new(Vec::new());
        for event in self.output_events()? {
            writer
                .write_event(event)
                .context("failed to serialize XMP packet")?;
        }
        Ok(writer.into_inner())
    }

    /// Builds the full output event stream by splicing the regenerated
    /// `dc:subject` property into the retained events.
    fn output_events(&self) -> Result<Vec<Event<'_>>> {
        let mut output: Vec<Event<'_>> = if self.keywords.is_empty() {
            self.retained_events(..).collect()
        } else {
            let placement = self.placement.context(
                "XMP packet offers no place for dc:subject: the rdf:RDF element is self-closing",
            )?;
            let declarations = self.namespaces.declarations.as_slice();
            let (index, replaces_event, block) = match placement {
                SubjectPlacement::Before(index) => {
                    (index, false, self.render_property(declarations))
                }
                SubjectPlacement::InsideSelfClosing(index) => {
                    (index, true, self.expand_description(index)?)
                }
                SubjectPlacement::WrappedBefore(index) => {
                    (index, false, self.wrap_in_description())
                }
            };

            let mut output: Vec<Event<'_>> = self.retained_events(..index).collect();
            output.extend(block);
            output.extend(self.retained_events(index + usize::from(replaces_event)..));
            output
        };
        if let Some(inference) = &self.geo_inference {
            let insertion_index = output
                .iter()
                .rposition(|event| {
                    matches!(event, Event::End(end) if end.local_name().into_inner() == b"RDF")
                })
                .context("XMP packet offers no rdf:RDF closing element for GPS metadata")?;
            output.splice(
                insertion_index..insertion_index,
                self.render_geo_inference(inference),
            );
        }
        Ok(output)
    }

    /// Borrows the retained events of the given range for serialization.
    fn retained_events<R>(&self, range: R) -> impl Iterator<Item = Event<'_>>
    where
        R: std::slice::SliceIndex<[ResolvedEvent], Output = [ResolvedEvent]>,
    {
        self.events[range].iter().map(|entry| entry.event.borrow())
    }

    /// Renders the `dc:subject` property, declaring the given namespaces on the
    /// property element itself.
    fn render_property(&self, declarations: &[NamespaceDeclaration]) -> Vec<Event<'static>> {
        let dc_prefix = &self.namespaces.dc_prefix;
        let rdf_prefix = &self.namespaces.rdf_prefix;

        let mut subject = BytesStart::new(qualified_name(dc_prefix, "subject"));
        for declaration in declarations {
            subject.push_attribute((declaration.attribute.as_str(), declaration.namespace));
        }

        // No capacity pre-allocation: `self.keywords.len()` can originate from
        // an externally supplied sidecar (a synced third-party `.xmp` file or a
        // `/store/labels` request), and reserving space proportional to it in
        // one allocation would let a crafted keyword list trigger a single
        // outsized allocation. `Vec::push` grows amortized instead.
        let mut events = Vec::new();
        self.push_line_break(&mut events, 1);
        events.push(Event::Start(subject));
        self.push_line_break(&mut events, 2);
        events.push(Event::Start(BytesStart::new(qualified_name(
            rdf_prefix, "Bag",
        ))));
        for keyword in &self.keywords {
            self.push_line_break(&mut events, 3);
            events.push(Event::Start(BytesStart::new(qualified_name(
                rdf_prefix, "li",
            ))));
            events.push(Event::Text(BytesText::new(keyword).into_owned()));
            events.push(Event::End(BytesEnd::new(qualified_name(rdf_prefix, "li"))));
        }
        self.push_line_break(&mut events, 2);
        events.push(Event::End(BytesEnd::new(qualified_name(rdf_prefix, "Bag"))));
        self.push_line_break(&mut events, 1);
        events.push(Event::End(BytesEnd::new(qualified_name(
            dc_prefix, "subject",
        ))));
        events
    }

    /// Expands the self-closing `rdf:Description` at `index` into an open element
    /// that carries the generated property.
    fn expand_description(&self, index: usize) -> Result<Vec<Event<'static>>> {
        let Event::Empty(start) = &self.events[index].event else {
            bail!("expected a self-closing rdf:Description at event {index}");
        };

        let mut events = vec![Event::Start(start.clone())];
        events.extend(self.render_property(&self.namespaces.declarations));
        self.push_line_break(&mut events, 0);
        events.push(Event::End(start.to_end().into_owned()));
        Ok(events)
    }

    /// Wraps the generated property in a fresh `rdf:Description` for packets that
    /// do not contain one yet.
    fn wrap_in_description(&self) -> Vec<Event<'static>> {
        let rdf_prefix = &self.namespaces.rdf_prefix;
        let mut description = BytesStart::new(qualified_name(rdf_prefix, "Description"));
        description.push_attribute((qualified_name(rdf_prefix, "about").as_str(), ""));
        for declaration in &self.namespaces.declarations {
            description.push_attribute((declaration.attribute.as_str(), declaration.namespace));
        }

        let mut events = Vec::new();
        self.push_line_break(&mut events, 0);
        events.push(Event::Start(description));
        events.extend(self.render_property(&[]));
        self.push_line_break(&mut events, 0);
        events.push(Event::End(BytesEnd::new(qualified_name(
            rdf_prefix,
            "Description",
        ))));
        events
    }

    fn render_geo_inference(&self, inference: &XmpGeoInference) -> Vec<Event<'static>> {
        let rdf_prefix = &self.namespaces.rdf_prefix;
        let mut description = BytesStart::new(qualified_name(rdf_prefix, "Description"));
        description.push_attribute((qualified_name(rdf_prefix, "about").as_str(), ""));
        description.push_attribute(("xmlns:exif", EXIF_NAMESPACE));
        description.push_attribute(("xmlns:berrykeep", BERRYKEEP_NAMESPACE));
        let latitude = format_xmp_coordinate(inference.latitude, true);
        let longitude = format_xmp_coordinate(inference.longitude, false);
        description.push_attribute((
            qualified_name(EXIF_PREFIX_FALLBACK, "GPSLatitude").as_str(),
            latitude.as_str(),
        ));
        description.push_attribute((
            qualified_name(EXIF_PREFIX_FALLBACK, "GPSLongitude").as_str(),
            longitude.as_str(),
        ));
        description.push_attribute((
            qualified_name(EXIF_PREFIX_FALLBACK, "GPSMapDatum").as_str(),
            "WGS-84",
        ));
        description.push_attribute((
            qualified_name(BERRYKEEP_PREFIX_FALLBACK, "GeoInferenceMethod").as_str(),
            inference.method.as_str(),
        ));
        description.push_attribute((
            qualified_name(BERRYKEEP_PREFIX_FALLBACK, "GeoInferenceRunId").as_str(),
            inference.run_id.as_str(),
        ));
        description.push_attribute((
            qualified_name(BERRYKEEP_PREFIX_FALLBACK, "GeoInferenceConfidence").as_str(),
            inference.confidence.as_str(),
        ));
        if let Some(value) = inference.reference_distance_seconds {
            description.push_attribute((
                qualified_name(
                    BERRYKEEP_PREFIX_FALLBACK,
                    "GeoInferenceReferenceDistanceSeconds",
                )
                .as_str(),
                value.to_string().as_str(),
            ));
        }
        if let Some(value) = inference.previous_anchor_distance_seconds {
            description.push_attribute((
                qualified_name(
                    BERRYKEEP_PREFIX_FALLBACK,
                    "GeoInferencePreviousAnchorDistanceSeconds",
                )
                .as_str(),
                value.to_string().as_str(),
            ));
        }
        if let Some(value) = inference.next_anchor_distance_seconds {
            description.push_attribute((
                qualified_name(
                    BERRYKEEP_PREFIX_FALLBACK,
                    "GeoInferenceNextAnchorDistanceSeconds",
                )
                .as_str(),
                value.to_string().as_str(),
            ));
        }
        if let Some(value) = inference.estimated_speed_kmh {
            let value = format!("{value:.3}");
            description.push_attribute((
                qualified_name(BERRYKEEP_PREFIX_FALLBACK, "GeoInferenceEstimatedSpeedKmh").as_str(),
                value.as_str(),
            ));
        }
        let mut events = Vec::new();
        self.push_line_break(&mut events, 0);
        events.push(Event::Empty(description));
        events
    }

    /// Appends a line break plus the indentation of the given nesting level,
    /// unless the packet is not indented at all.
    fn push_line_break(&self, events: &mut Vec<Event<'static>>, depth: usize) {
        if let Some(indent) = &self.indent {
            events.push(Event::Text(BytesText::from_escaped(indent.line(depth))));
        }
    }
}

/// A raw XML event together with the namespace URI its element name resolves to.
///
/// The raw event allows byte-faithful re-serialization of content Ironmesh does
/// not model, while the resolved namespace allows locating our own properties
/// regardless of the prefixes the authoring tool happened to pick.
#[derive(Debug, Clone)]
struct ResolvedEvent {
    event: Event<'static>,
    namespace: Option<String>,
    attributes: Vec<ResolvedAttribute>,
}

/// An attribute name resolved in the namespace scope of its owning event.
///
/// `quick_xml` resolves element names for us, but XMP uses many properties as
/// attributes. Retaining their expanded names avoids treating an unrelated
/// `foo:GPSLatitude` as EXIF merely because some other part of the document
/// happens to bind an `exif` prefix.
#[derive(Debug, Clone)]
struct ResolvedAttribute {
    raw_name: Vec<u8>,
    namespace: Option<String>,
    local_name: Vec<u8>,
}

/// Span of an element inside the event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ElementSpan {
    /// Index of the `Start` or `Empty` event.
    start: usize,
    /// Index of the matching `End` event, or `start` for a self-closing element.
    end: usize,
    /// Whether the element was written as `<name/>`.
    self_closing: bool,
}

impl ElementSpan {
    fn new(start: usize, self_closing: bool) -> Self {
        Self {
            start,
            end: start,
            self_closing,
        }
    }
}

/// Elements of an XMP packet that are relevant for storing `dc:subject`.
#[derive(Debug, Default, Clone, Copy)]
struct PacketStructure {
    /// The `rdf:RDF` root element.
    rdf_root: Option<ElementSpan>,
    /// The first `rdf:Description` directly below `rdf:RDF`.
    description: Option<ElementSpan>,
    /// The first child element of that `rdf:Description`.
    description_first_child: Option<usize>,
    /// The first `dc:subject` property of any top-level `rdf:Description`.
    subject: Option<ElementSpan>,
    /// The top-level `rdf:Description` that contains [`Self::subject`].
    subject_description: Option<usize>,
}

/// Position at which the regenerated `dc:subject` property is spliced into the
/// retained event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubjectPlacement {
    /// Insert the property immediately before the event at this index.
    Before(usize),
    /// Replace the self-closing `rdf:Description` at this index with an open
    /// element that contains the property.
    InsideSelfClosing(usize),
    /// Wrap the property in a new `rdf:Description` and insert it immediately
    /// before the event at this index.
    WrappedBefore(usize),
}

/// Result of analysing where `dc:subject` lives in a packet.
#[derive(Debug, Clone)]
struct PacketLayout {
    /// `None` when the packet cannot hold a property at all.
    placement: Option<SubjectPlacement>,
    /// Events of an already present `dc:subject` property, including its leading
    /// indentation, that have to be dropped from the retained stream.
    replaced: Option<Range<usize>>,
    /// Whitespace layout of the packet.
    indent: Option<Indentation>,
}

/// Whitespace layout of a packet, reused so that edited sidecars keep the
/// formatting of the authoring tool.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Indentation {
    /// Indentation of the `rdf:Description` element.
    base: String,
    /// One additional nesting level.
    step: String,
}

impl Indentation {
    /// Line break followed by the indentation `depth` levels below
    /// `rdf:Description`.
    fn line(&self, depth: usize) -> String {
        format!("\n{}{}", self.base, self.step.repeat(depth))
    }
}

/// A namespace declaration that generated markup has to carry because the packet
/// does not bind that namespace yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NamespaceDeclaration {
    /// Attribute name, for example `xmlns:dc`.
    attribute: String,
    /// Namespace URI the prefix is bound to.
    namespace: &'static str,
}

/// Prefixes used for generated markup plus the declarations it has to emit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GeneratedNamespaces {
    dc_prefix: String,
    rdf_prefix: String,
    declarations: Vec<NamespaceDeclaration>,
}

/// Reads every event of the packet, keeping the raw bytes and the resolved
/// namespace of each element.
fn read_resolved_events(bytes: &[u8]) -> Result<Vec<ResolvedEvent>> {
    let mut reader = NsReader::from_reader(bytes);
    let mut events = Vec::new();
    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .context("failed to parse XMP packet")?;
        if matches!(event, Event::Eof) {
            break;
        }
        let namespace = resolved_namespace(resolved);
        let attributes = match &event {
            Event::Start(element) | Event::Empty(element) => {
                let mut attributes = element.attributes();
                attributes.with_checks(false);
                attributes
                    .flatten()
                    .map(|attribute| {
                        let (namespace, local_name) =
                            reader.resolver().resolve_attribute(attribute.key);
                        ResolvedAttribute {
                            raw_name: attribute.key.as_ref().to_vec(),
                            namespace: resolved_namespace(namespace),
                            local_name: local_name.as_ref().to_vec(),
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        events.push(ResolvedEvent {
            namespace,
            attributes,
            event: event.into_owned(),
        });
    }
    Ok(events)
}

fn resolved_namespace(resolved: ResolveResult<'_>) -> Option<String> {
    match resolved {
        ResolveResult::Bound(namespace) => {
            Some(String::from_utf8_lossy(namespace.into_inner()).into_owned())
        }
        ResolveResult::Unbound | ResolveResult::Unknown(_) => None,
    }
}

/// Locates the elements that matter for storing `dc:subject`.
fn scan_structure(events: &[ResolvedEvent]) -> PacketStructure {
    let mut structure = PacketStructure::default();
    let mut open: Vec<usize> = Vec::new();
    for (index, resolved) in events.iter().enumerate() {
        match resolved.event {
            Event::Start(_) => {
                note_element_start(&mut structure, events, &open, index);
                open.push(index);
            }
            Event::Empty(_) => note_element_start(&mut structure, events, &open, index),
            Event::End(_) => {
                if let Some(start) = open.pop() {
                    note_element_end(&mut structure, start, index);
                }
            }
            _ => {}
        }
    }
    structure
}

/// Records an opening element if it is one of the elements we care about.
fn note_element_start(
    structure: &mut PacketStructure,
    events: &[ResolvedEvent],
    open: &[usize],
    index: usize,
) {
    let resolved = &events[index];
    let self_closing = matches!(resolved.event, Event::Empty(_));

    if structure.rdf_root.is_none() {
        if is_element(resolved, RDF_NAMESPACE, b"RDF") {
            structure.rdf_root = Some(ElementSpan::new(index, self_closing));
        }
        return;
    }
    let Some(rdf_root) = structure.rdf_root else {
        return;
    };
    let parent = open.last().copied();

    if parent == Some(rdf_root.start) {
        if structure.description.is_none() && is_element(resolved, RDF_NAMESPACE, b"Description") {
            structure.description = Some(ElementSpan::new(index, self_closing));
        }
        return;
    }

    if parent == structure.description.map(|description| description.start)
        && structure.description_first_child.is_none()
    {
        structure.description_first_child = Some(index);
    }

    let grandparent = open.len().checked_sub(2).map(|position| open[position]);
    let below_top_level_description = grandparent == Some(rdf_root.start)
        && parent.is_some_and(|parent| is_element(&events[parent], RDF_NAMESPACE, b"Description"));
    if structure.subject.is_none()
        && below_top_level_description
        && is_element(resolved, DC_NAMESPACE, b"subject")
    {
        structure.subject = Some(ElementSpan::new(index, self_closing));
        structure.subject_description = parent;
    }
}

/// Completes the span of an element that was recorded by [`note_element_start`].
fn note_element_end(structure: &mut PacketStructure, start: usize, end: usize) {
    for span in [
        &mut structure.rdf_root,
        &mut structure.description,
        &mut structure.subject,
    ] {
        if let Some(span) = span.as_mut()
            && span.start == start
        {
            span.end = end;
        }
    }
}

/// Reads the `rdf:li` entries of a `dc:subject` property.
fn read_keywords(events: &[ResolvedEvent], subject: ElementSpan) -> Result<Vec<String>> {
    let mut keywords = Vec::new();
    let mut item: Option<String> = None;
    for resolved in &events[subject.start..=subject.end] {
        let is_item = is_element(resolved, RDF_NAMESPACE, b"li");
        match &resolved.event {
            Event::Start(_) if is_item => item = Some(String::new()),
            Event::Empty(_) if is_item => keywords.push(String::new()),
            Event::End(_) if is_item => {
                if let Some(value) = item.take() {
                    keywords.push(value);
                }
            }
            Event::Text(text) => push_item_text(&mut item, text.xml10_content()?.as_ref()),
            Event::CData(data) => push_item_text(&mut item, decode_cdata(data)?.as_ref()),
            Event::GeneralRef(reference) => {
                push_item_text(&mut item, &resolve_reference(reference)?);
            }
            _ => {}
        }
    }
    Ok(keywords)
}

/// Appends decoded character data to the keyword currently being read.
fn push_item_text(item: &mut Option<String>, text: &str) {
    if let Some(item) = item.as_mut() {
        item.push_str(text);
    }
}

/// Decodes the payload of a CDATA section.
fn decode_cdata(data: &BytesCData<'_>) -> Result<String> {
    Ok(data
        .decode()
        .context("failed to decode CDATA in XMP packet")?
        .into_owned())
}

/// Resolves a character or predefined entity reference to its text.
fn resolve_reference(reference: &BytesRef<'_>) -> Result<String> {
    if let Some(character) = reference
        .resolve_char_ref()
        .context("failed to resolve character reference in XMP packet")?
    {
        return Ok(character.to_string());
    }
    let name = reference
        .decode()
        .context("failed to decode entity reference in XMP packet")?;
    resolve_predefined_entity(name.as_ref())
        .map(str::to_owned)
        .with_context(|| format!("unsupported entity reference in XMP packet: &{name};"))
}

/// Decides where the regenerated `dc:subject` property has to be written.
fn plan_layout(events: &[ResolvedEvent], structure: &PacketStructure) -> PacketLayout {
    let indent = derive_indentation(events, structure);

    if let Some(subject) = structure.subject {
        let start = line_start_index(events, subject.start);
        return PacketLayout {
            placement: Some(SubjectPlacement::Before(start)),
            replaced: Some(start..subject.end + 1),
            indent,
        };
    }

    if let Some(description) = structure.description {
        let placement = if description.self_closing {
            SubjectPlacement::InsideSelfClosing(description.start)
        } else {
            SubjectPlacement::Before(line_start_index(events, description.end))
        };
        return PacketLayout {
            placement: Some(placement),
            replaced: None,
            indent,
        };
    }

    let placement = structure
        .rdf_root
        .filter(|rdf_root| !rdf_root.self_closing)
        .map(|rdf_root| SubjectPlacement::WrappedBefore(line_start_index(events, rdf_root.end)));
    PacketLayout {
        placement,
        replaced: None,
        indent,
    }
}

/// Derives the whitespace layout from the indentation the packet already uses.
fn derive_indentation(
    events: &[ResolvedEvent],
    structure: &PacketStructure,
) -> Option<Indentation> {
    let base = structure
        .description
        .and_then(|description| preceding_line_indent(events, description.start))
        .or_else(|| {
            let rdf_root = structure.rdf_root?;
            let indent = preceding_line_indent(events, rdf_root.end)?;
            Some(indent + INDENT_STEP_FALLBACK)
        })?;
    let step = structure
        .description_first_child
        .and_then(|child| preceding_line_indent(events, child))
        .and_then(|child_indent| child_indent.strip_prefix(base.as_str()).map(str::to_owned))
        .filter(|step| !step.is_empty())
        .unwrap_or_else(|| INDENT_STEP_FALLBACK.to_owned());
    Some(Indentation { base, step })
}

/// Index at which the line of the event at `index` starts, so that generated
/// markup replaces the indentation in front of the event as well.
fn line_start_index(events: &[ResolvedEvent], index: usize) -> usize {
    match index.checked_sub(1) {
        Some(previous) if preceding_line_indent(events, index).is_some() => previous,
        _ => index,
    }
}

/// Indentation of the line the event at `index` starts on, taken from the
/// whitespace-only text node in front of it.
fn preceding_line_indent(events: &[ResolvedEvent], index: usize) -> Option<String> {
    let previous = events.get(index.checked_sub(1)?)?;
    let Event::Text(text) = &previous.event else {
        return None;
    };
    let content = text.xml10_content().ok()?;
    if !content.chars().all(char::is_whitespace) {
        return None;
    }
    let (_, indent) = content.rsplit_once('\n')?;
    Some(indent.to_owned())
}

/// Picks the prefixes for generated markup, falling back to freshly declared
/// ones when the packet does not bind a namespace yet.
///
/// Only elements that are actual ancestors of the generated `dc:subject` --
/// `rdf:RDF` and its target `rdf:Description` -- are searched. When an
/// existing subject is regenerated, its containing description is the target;
/// otherwise the first description is. A namespace bound on an unrelated
/// element elsewhere in the packet, for example a sibling `rdf:Description`,
/// is not in scope at the insertion point: using its prefix without
/// redeclaring it would emit an unbound prefix.
fn generated_namespaces(
    events: &[ResolvedEvent],
    structure: &PacketStructure,
) -> GeneratedNamespaces {
    let scope: Vec<usize> = [
        structure.rdf_root.map(|span| span.start),
        structure
            .subject_description
            .or(structure.description.map(|span| span.start)),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut declarations = Vec::new();
    let mut prefix_for = |namespace: &'static str, fallback: &str| {
        declared_prefix(events, &scope, namespace).unwrap_or_else(|| {
            declarations.push(NamespaceDeclaration {
                attribute: format!("xmlns:{fallback}"),
                namespace,
            });
            fallback.to_owned()
        })
    };

    let dc_prefix = prefix_for(DC_NAMESPACE, DC_PREFIX_FALLBACK);
    let rdf_prefix = prefix_for(RDF_NAMESPACE, RDF_PREFIX_FALLBACK);
    GeneratedNamespaces {
        dc_prefix,
        rdf_prefix,
        declarations,
    }
}

/// Finds the prefix bound to `namespace` on one of the given scope elements.
fn declared_prefix(events: &[ResolvedEvent], scope: &[usize], namespace: &str) -> Option<String> {
    scope.iter().find_map(|&index| match &events[index].event {
        Event::Start(element) | Event::Empty(element) => prefix_declaration(element, namespace),
        _ => None,
    })
}

/// Extracts the prefix an element declares for the given namespace.
fn prefix_declaration(element: &BytesStart<'_>, namespace: &str) -> Option<String> {
    let mut attributes = element.attributes();
    attributes.with_checks(false);
    attributes.flatten().find_map(|attribute| {
        let prefix = attribute.key.as_ref().strip_prefix(b"xmlns:")?;
        (attribute.value.as_ref() == namespace.as_bytes())
            .then(|| String::from_utf8_lossy(prefix).into_owned())
    })
}

/// Returns `true` when the event is a start, empty or end tag of the given
/// namespace and local name.
fn is_element(resolved: &ResolvedEvent, namespace: &str, local_name: &[u8]) -> bool {
    resolved.namespace.as_deref() == Some(namespace)
        && element_local_name(&resolved.event) == Some(local_name)
}

/// Local name of an element event, ignoring its prefix.
fn element_local_name<'a>(event: &'a Event<'_>) -> Option<&'a [u8]> {
    match event {
        Event::Start(element) | Event::Empty(element) => Some(element.local_name().into_inner()),
        Event::End(element) => Some(element.local_name().into_inner()),
        _ => None,
    }
}

/// Joins a namespace prefix and a local name into a qualified XML name.
fn qualified_name(prefix: &str, local_name: &str) -> String {
    format!("{prefix}:{local_name}")
}

fn read_geo_location(events: &[ResolvedEvent]) -> Option<XmpGeoLocation> {
    let inferred_by_berrykeep = has_berrykeep_inference_marker(events);
    let mut location = None;
    let mut description_coordinates: Option<(Option<f64>, Option<f64>)> = None;
    let mut element_property = None;
    for event in events {
        match &event.event {
            Event::Start(element) | Event::Empty(element) => {
                if is_element(event, RDF_NAMESPACE, b"Description") {
                    let coordinates = geo_attributes(event, element);
                    if matches!(&event.event, Event::Empty(_)) {
                        set_first_geo_location(&mut location, coordinates);
                    } else {
                        description_coordinates = Some(coordinates);
                    }
                } else if matches!(&event.event, Event::Start(_))
                    && description_coordinates.is_some()
                {
                    element_property = if is_element(event, EXIF_NAMESPACE, b"GPSLatitude") {
                        Some(true)
                    } else if is_element(event, EXIF_NAMESPACE, b"GPSLongitude") {
                        Some(false)
                    } else {
                        None
                    };
                }
            }
            Event::Text(text) => {
                let Some(is_latitude) = element_property else {
                    continue;
                };
                let Ok(value) = text.xml10_content() else {
                    continue;
                };
                if let Some((latitude, longitude)) = description_coordinates.as_mut() {
                    if is_latitude {
                        *latitude = parse_xmp_coordinate(&value, true);
                    } else {
                        *longitude = parse_xmp_coordinate(&value, false);
                    }
                }
            }
            Event::CData(text) => {
                let Some(is_latitude) = element_property else {
                    continue;
                };
                let Ok(value) = std::str::from_utf8(text.as_ref()) else {
                    continue;
                };
                if let Some((latitude, longitude)) = description_coordinates.as_mut() {
                    if is_latitude {
                        *latitude = parse_xmp_coordinate(value, true);
                    } else {
                        *longitude = parse_xmp_coordinate(value, false);
                    }
                }
            }
            Event::End(_) => {
                if element_property.is_some()
                    && (is_element(event, EXIF_NAMESPACE, b"GPSLatitude")
                        || is_element(event, EXIF_NAMESPACE, b"GPSLongitude"))
                {
                    element_property = None;
                }
                if is_element(event, RDF_NAMESPACE, b"Description") {
                    set_first_geo_location(
                        &mut location,
                        description_coordinates.take().unwrap_or_default(),
                    );
                }
            }
            _ => {}
        }
    }
    location.map(|(latitude, longitude)| XmpGeoLocation {
        latitude,
        longitude,
        inferred_by_berrykeep,
    })
}

fn geo_attributes(
    resolved: &ResolvedEvent,
    element: &BytesStart<'_>,
) -> (Option<f64>, Option<f64>) {
    let mut latitude = None;
    let mut longitude = None;
    let mut attributes = element.attributes();
    attributes.with_checks(false);
    for attribute in attributes.flatten() {
        let Some(name) = resolved
            .attributes
            .iter()
            .find(|name| name.raw_name == attribute.key.as_ref())
        else {
            continue;
        };
        if name.namespace.as_deref() != Some(EXIF_NAMESPACE) {
            continue;
        }
        let Ok(value) = std::str::from_utf8(attribute.value.as_ref()) else {
            continue;
        };
        match name.local_name.as_slice() {
            b"GPSLatitude" => latitude = parse_xmp_coordinate(value, true),
            b"GPSLongitude" => longitude = parse_xmp_coordinate(value, false),
            _ => {}
        }
    }
    (latitude, longitude)
}

fn set_first_geo_location(
    location: &mut Option<(f64, f64)>,
    coordinates: (Option<f64>, Option<f64>),
) {
    if location.is_some() {
        return;
    }
    match coordinates {
        (Some(latitude), Some(longitude))
            if (-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude) =>
        {
            *location = Some((latitude, longitude));
        }
        _ => {}
    }
}

/// Detects the provenance written by [`XmpSidecar::set_geo_inference`].
/// Attribute prefixes are resolved from declarations in the packet so a
/// third-party writer may use a prefix other than `berrykeep`.
fn has_berrykeep_inference_marker(events: &[ResolvedEvent]) -> bool {
    events.iter().any(|resolved| {
        if is_element(resolved, BERRYKEEP_NAMESPACE, b"GeoInferenceMethod") {
            return true;
        }
        resolved.attributes.iter().any(|attribute| {
            attribute.namespace.as_deref() == Some(BERRYKEEP_NAMESPACE)
                && attribute.local_name == b"GeoInferenceMethod"
        })
    })
}

fn parse_xmp_coordinate(value: &str, latitude: bool) -> Option<f64> {
    let value = value.trim();
    let hemisphere = value.chars().last()?;
    let numeric = value.strip_suffix(hemisphere)?.trim();
    let sign = match hemisphere {
        'N' | 'n' if latitude => 1.0,
        'S' | 's' if latitude => -1.0,
        'E' | 'e' if !latitude => 1.0,
        'W' | 'w' if !latitude => -1.0,
        _ => return None,
    };
    let mut components = numeric.trim().split(',').map(str::trim);
    let degrees = components.next()?.parse::<f64>().ok()?;
    let minutes = components.next()?.parse::<f64>().ok()?;
    let seconds = components
        .next()
        .map(str::parse::<f64>)
        .transpose()
        .ok()?
        .unwrap_or_default();
    if components.next().is_some()
        || !degrees.is_finite()
        || !minutes.is_finite()
        || !seconds.is_finite()
        || degrees < 0.0
        || !(0.0..60.0).contains(&minutes)
        || !(0.0..60.0).contains(&seconds)
    {
        return None;
    }
    let value = degrees + minutes / 60.0 + seconds / 3_600.0;
    let limit = if latitude { 90.0 } else { 180.0 };
    (value.is_finite() && value <= limit).then_some(value * sign)
}

fn format_xmp_coordinate(value: f64, latitude: bool) -> String {
    let hemisphere = match (latitude, value.is_sign_negative()) {
        (true, false) => 'N',
        (true, true) => 'S',
        (false, false) => 'E',
        (false, true) => 'W',
    };
    let absolute = value.abs();
    let mut degrees = absolute.floor();
    let mut minutes = (absolute - degrees) * 60.0;
    // Do not emit an invalid `60.000000` minute component due to rounding.
    if minutes >= 59.999_999_5 {
        degrees += 1.0;
        minutes = 0.0;
    }
    format!("{degrees:.0},{minutes:.6}{hemisphere}")
}

/// Derives the sidecar key of a media object: `album/photo.jpg` ->
/// `album/photo.jpg.xmp`.
pub fn sidecar_key_for_media(key: &str) -> String {
    format!("{key}{XMP_SIDECAR_SUFFIX}")
}

/// Derives the media key of a sidecar object: `album/photo.jpg.xmp` ->
/// `album/photo.jpg`.
///
/// Returns `None` for keys that are not sidecars, including keys whose file name
/// is exactly `.xmp`. The suffix is matched case-sensitively so that
/// [`sidecar_key_for_media`] and this function are exact inverses.
pub fn media_key_for_sidecar(key: &str) -> Option<String> {
    let media_key = key.strip_suffix(XMP_SIDECAR_SUFFIX)?;
    if media_key.is_empty() || media_key.ends_with(['/', '\\']) {
        return None;
    }
    Some(media_key.to_owned())
}

/// Returns `true` when the key names an XMP sidecar object.
pub fn is_sidecar_key(key: &str) -> bool {
    media_key_for_sidecar(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::{
        DC_NAMESPACE, XmpGeoInference, XmpSidecar, is_sidecar_key, media_key_for_sidecar,
        sidecar_key_for_media,
    };
    use anyhow::Result;

    /// Sidecar in the shape Lightroom writes it: many namespaces Ironmesh does
    /// not model, plus a `dc:subject` property it does.
    const THIRD_PARTY_SIDECAR: &str = concat!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Adobe XMP Core 5.6-c145\">\n",
        " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
        "  <rdf:Description rdf:about=\"\"\n",
        "    xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n",
        "    xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n",
        "    xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"\n",
        "    xmlns:photoshop=\"http://ns.adobe.com/photoshop/1.0/\"\n",
        "    xmlns:xmpMM=\"http://ns.adobe.com/xap/1.0/mm/\"\n",
        "    xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n",
        "   xmp:Rating=\"3\"\n",
        "   xmp:ModifyDate=\"2024-05-11T14:22:31+02:00\"\n",
        "   crs:Version=\"12.2.1\"\n",
        "   crs:Exposure2012=\"+0.35\"\n",
        "   photoshop:DateCreated=\"2024-05-11T09:03:12\"\n",
        "   xmpMM:DocumentID=\"xmp.did:0d4f7a2c-1e5b-4f9a-9a3e-2b7d5e8c1f60\"\n",
        "   exif:ISOSpeedRatings=\"400\">\n",
        "   <dc:subject>\n",
        "    <rdf:Bag>\n",
        "     <rdf:li>vacation</rdf:li>\n",
        "     <rdf:li>beach</rdf:li>\n",
        "    </rdf:Bag>\n",
        "   </dc:subject>\n",
        "   <crs:ToneCurvePV2012>\n",
        "    <rdf:Seq>\n",
        "     <rdf:li>0, 0</rdf:li>\n",
        "     <rdf:li>255, 255</rdf:li>\n",
        "    </rdf:Seq>\n",
        "   </crs:ToneCurvePV2012>\n",
        "   <xmpMM:History>\n",
        "    <rdf:Seq>\n",
        "     <rdf:li rdf:parseType=\"Resource\">\n",
        "      <stEvt:action xmlns:stEvt=\"http://ns.adobe.com/xap/1.0/sType/ResourceEvent#\">derived</stEvt:action>\n",
        "     </rdf:li>\n",
        "    </rdf:Seq>\n",
        "   </xmpMM:History>\n",
        "   <!-- kept verbatim -->\n",
        "  </rdf:Description>\n",
        " </rdf:RDF>\n",
        "</x:xmpmeta>\n",
        "<?xpacket end=\"w\"?>\n",
    );

    /// Sidecar without any `dc:subject` property.
    const SIDECAR_WITHOUT_KEYWORDS: &str = concat!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"darktable\">\n",
        " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
        "  <rdf:Description rdf:about=\"\"\n",
        "    xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n",
        "    xmlns:darktable=\"http://darktable.sf.net/\"\n",
        "   xmp:Rating=\"5\"\n",
        "   darktable:import_timestamp=\"1715412345\">\n",
        "   <darktable:history>\n",
        "    <rdf:Seq>\n",
        "     <rdf:li darktable:operation=\"flip\"/>\n",
        "    </rdf:Seq>\n",
        "   </darktable:history>\n",
        "  </rdf:Description>\n",
        " </rdf:RDF>\n",
        "</x:xmpmeta>\n",
        "<?xpacket end=\"w\"?>\n",
    );

    /// Removes the `dc:subject` property including the indentation in front of it
    /// so that the remaining bytes can be compared for exact equality.
    fn without_subject_property(document: &str) -> String {
        const CLOSING_TAG: &str = "</dc:subject>";
        let Some(open) = document.find("<dc:subject") else {
            return document.to_owned();
        };
        let start = document[..open].rfind('\n').unwrap_or(open);
        assert!(
            document[start..open].chars().all(char::is_whitespace),
            "dc:subject is expected to start on its own line"
        );
        let end = document.find(CLOSING_TAG).expect("closing dc:subject tag") + CLOSING_TAG.len();
        format!("{}{}", &document[..start], &document[end..])
    }

    fn serialize(sidecar: &XmpSidecar) -> Result<String> {
        Ok(String::from_utf8(sidecar.to_bytes()?)?)
    }

    #[test]
    fn third_party_sidecar_round_trips_unknown_namespaces() -> Result<()> {
        let mut sidecar = XmpSidecar::parse(THIRD_PARTY_SIDECAR.as_bytes())?;
        sidecar.set_keywords(vec!["private".to_owned(), "nsfw".to_owned()]);
        let serialized = serialize(&sidecar)?;

        for retained in [
            "xmp:Rating=\"3\"",
            "xmp:ModifyDate=\"2024-05-11T14:22:31+02:00\"",
            "crs:Version=\"12.2.1\"",
            "crs:Exposure2012=\"+0.35\"",
            "photoshop:DateCreated=\"2024-05-11T09:03:12\"",
            "xmpMM:DocumentID=\"xmp.did:0d4f7a2c-1e5b-4f9a-9a3e-2b7d5e8c1f60\"",
            "exif:ISOSpeedRatings=\"400\"",
            "<crs:ToneCurvePV2012>",
            "<rdf:li>255, 255</rdf:li>",
            "<stEvt:action xmlns:stEvt=\"http://ns.adobe.com/xap/1.0/sType/ResourceEvent#\">derived</stEvt:action>",
            "<!-- kept verbatim -->",
            "<?xpacket end=\"w\"?>",
        ] {
            assert!(
                serialized.contains(retained),
                "expected serialized packet to retain {retained:?}, got:\n{serialized}"
            );
        }

        assert_eq!(
            without_subject_property(THIRD_PARTY_SIDECAR),
            without_subject_property(&serialized),
            "everything outside dc:subject must be byte identical"
        );

        let reparsed = XmpSidecar::parse(serialized.as_bytes())?;
        assert_eq!(reparsed.keywords(), ["private", "nsfw"]);
        Ok(())
    }

    #[test]
    fn parse_reads_existing_keywords() -> Result<()> {
        let sidecar = XmpSidecar::parse(THIRD_PARTY_SIDECAR.as_bytes())?;
        assert_eq!(sidecar.keywords(), ["vacation", "beach"]);
        Ok(())
    }

    #[test]
    fn keywords_can_be_added_to_a_sidecar_without_any() -> Result<()> {
        let mut sidecar = XmpSidecar::parse(SIDECAR_WITHOUT_KEYWORDS.as_bytes())?;
        assert!(sidecar.keywords().is_empty());

        sidecar.set_keywords(vec!["private".to_owned()]);
        let serialized = serialize(&sidecar)?;

        assert!(
            serialized.contains("xmlns:dc=\"http://purl.org/dc/elements/1.1/\""),
            "the dc namespace must be declared, got:\n{serialized}"
        );
        assert!(
            serialized.contains("<darktable:history>"),
            "unknown properties must survive, got:\n{serialized}"
        );
        assert_eq!(
            without_subject_property(SIDECAR_WITHOUT_KEYWORDS),
            without_subject_property(&serialized),
            "adding a property must not touch anything else"
        );

        let reparsed = XmpSidecar::parse(serialized.as_bytes())?;
        assert_eq!(reparsed.keywords(), ["private"]);
        Ok(())
    }

    #[test]
    fn empty_packet_accepts_keywords() -> Result<()> {
        let mut sidecar = XmpSidecar::new_empty();
        assert!(sidecar.keywords().is_empty());

        sidecar.set_keywords(vec!["private".to_owned(), "nsfw".to_owned()]);
        let serialized = serialize(&sidecar)?;

        let reparsed = XmpSidecar::parse(serialized.as_bytes())?;
        assert_eq!(reparsed.keywords(), ["private", "nsfw"]);
        assert_eq!(serialize(&reparsed)?, serialized, "writing must be stable");
        Ok(())
    }

    #[test]
    fn geolocation_write_preserves_unknown_xmp_and_records_provenance() -> Result<()> {
        let mut sidecar = XmpSidecar::parse(THIRD_PARTY_SIDECAR.as_bytes())?;
        sidecar.set_geo_inference(XmpGeoInference {
            latitude: 37.808_333_33,
            longitude: -122.404_166_67,
            method: "interpolation".to_string(),
            run_id: "run-123".to_string(),
            confidence: "previous=120s; next=180s; estimated_speed=5.0km/h".to_string(),
            reference_distance_seconds: None,
            previous_anchor_distance_seconds: Some(120),
            next_anchor_distance_seconds: Some(180),
            estimated_speed_kmh: Some(5.0),
        })?;
        let serialized = serialize(&sidecar)?;
        for retained in [
            "xmp:Rating=\"3\"",
            "crs:Exposure2012=\"+0.35\"",
            "<crs:ToneCurvePV2012>",
            "<!-- kept verbatim -->",
        ] {
            assert!(serialized.contains(retained), "missing {retained:?}");
        }
        assert!(serialized.contains("exif:GPSLatitude=\"37,48.500000N\""));
        assert!(serialized.contains("exif:GPSLongitude=\"122,24.250000W\""));
        assert!(serialized.contains("exif:GPSMapDatum=\"WGS-84\""));
        assert!(serialized.contains("berrykeep:GeoInferenceRunId=\"run-123\""));
        let reparsed = XmpSidecar::parse(serialized.as_bytes())?;
        let location = reparsed.geo_location().expect("GPS must round-trip");
        assert!((location.latitude - 37.808_333_33).abs() < 0.000_001);
        assert!((location.longitude + 122.404_166_67).abs() < 0.000_001);
        assert!(location.inferred_by_berrykeep);
        Ok(())
    }

    #[test]
    fn reads_existing_element_form_gps() -> Result<()> {
        let packet = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description xmlns:exif=\"http://ns.adobe.com/exif/1.0/\">",
            "<exif:GPSLatitude>47,22,36N</exif:GPSLatitude>",
            "<exif:GPSLongitude>8,32,30E</exif:GPSLongitude>",
            "</rdf:Description></rdf:RDF></x:xmpmeta>"
        );
        let sidecar = XmpSidecar::parse(packet.as_bytes())?;
        let location = sidecar.geo_location().expect("element GPS must be read");
        assert!((location.latitude - 47.376_666_666_7).abs() < 0.000_001);
        assert!((location.longitude - 8.541_666_666_7).abs() < 0.000_001);
        assert!(!location.inferred_by_berrykeep);
        Ok(())
    }

    #[test]
    fn geolocation_requires_a_hemisphere_and_xmp_coordinate_components() -> Result<()> {
        let packet = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description xmlns:exif=\"http://ns.adobe.com/exif/1.0/\" ",
            "exif:GPSLatitude=\"47.3769\" exif:GPSLongitude=\"8,32.5E\"/>",
            "</rdf:RDF></x:xmpmeta>"
        );
        let sidecar = XmpSidecar::parse(packet.as_bytes())?;
        assert!(sidecar.geo_location().is_none());
        Ok(())
    }

    #[test]
    fn geolocation_attributes_are_exif_scoped_and_never_combined_across_descriptions() -> Result<()>
    {
        let packet = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description xmlns:exif=\"https://example.invalid/\" ",
            "exif:GPSLatitude=\"47,22N\" exif:GPSLongitude=\"8,32E\"/>",
            "<rdf:Description xmlns:exif=\"http://ns.adobe.com/exif/1.0/\" ",
            "exif:GPSLatitude=\"47,22N\"/>",
            "<rdf:Description xmlns:exif=\"http://ns.adobe.com/exif/1.0/\" ",
            "exif:GPSLongitude=\"8,32E\"/>",
            "</rdf:RDF></x:xmpmeta>"
        );
        let sidecar = XmpSidecar::parse(packet.as_bytes())?;
        assert!(
            sidecar.geo_location().is_none(),
            "a location must be a complete EXIF pair on one rdf:Description"
        );
        Ok(())
    }

    #[test]
    fn clearing_keywords_removes_the_property() -> Result<()> {
        let mut sidecar = XmpSidecar::parse(THIRD_PARTY_SIDECAR.as_bytes())?;
        sidecar.set_keywords(Vec::new());
        let serialized = serialize(&sidecar)?;

        assert!(!serialized.contains("dc:subject"));
        assert_eq!(
            without_subject_property(THIRD_PARTY_SIDECAR),
            serialized,
            "removing the property must not touch anything else"
        );
        assert!(
            XmpSidecar::parse(serialized.as_bytes())?
                .keywords()
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn third_party_prefixes_are_reused_instead_of_redeclared() -> Result<()> {
        const ALTERNATE_PREFIXES: &str = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <r:RDF xmlns:r=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <r:Description r:about=\"\" xmlns:dcx=\"http://purl.org/dc/elements/1.1/\">\n",
            "   <dcx:subject>\n",
            "    <r:Bag>\n",
            "     <r:li>alpha</r:li>\n",
            "    </r:Bag>\n",
            "   </dcx:subject>\n",
            "  </r:Description>\n",
            " </r:RDF>\n",
            "</x:xmpmeta>\n",
        );

        let mut sidecar = XmpSidecar::parse(ALTERNATE_PREFIXES.as_bytes())?;
        assert_eq!(sidecar.keywords(), ["alpha"]);

        sidecar.set_keywords(vec!["beta".to_owned()]);
        let serialized = serialize(&sidecar)?;

        assert!(
            serialized.contains("<dcx:subject>") && serialized.contains("<r:li>beta</r:li>"),
            "the prefixes of the packet must be reused, got:\n{serialized}"
        );
        assert!(
            !serialized.contains("xmlns:dc="),
            "an already bound namespace must not be redeclared, got:\n{serialized}"
        );
        assert_eq!(
            XmpSidecar::parse(serialized.as_bytes())?.keywords(),
            ["beta"]
        );
        Ok(())
    }

    /// A namespace bound on a *sibling* `rdf:Description` is not in scope at
    /// the insertion point. exiftool and other tools regularly split
    /// properties across several top-level `rdf:Description` elements; the
    /// first one -- where `dc:subject` is inserted -- must not borrow a prefix
    /// declared only on a later one.
    #[test]
    fn dc_prefix_bound_only_on_a_sibling_description_is_not_reused() -> Result<()> {
        const SPLIT_ACROSS_DESCRIPTIONS: &str = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"3\"/>\n",
            "  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n",
            "   <dc:title>t</dc:title>\n",
            "  </rdf:Description>\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
        );

        let mut sidecar = XmpSidecar::parse(SPLIT_ACROSS_DESCRIPTIONS.as_bytes())?;
        assert!(sidecar.keywords().is_empty());

        sidecar.set_keywords(vec!["private".to_owned()]);
        let serialized = serialize(&sidecar)?;

        assert!(
            serialized.contains(&format!("<dc:subject xmlns:dc=\"{DC_NAMESPACE}\">")),
            "dc:subject must declare its own namespace since the sibling description's \
             binding is not in scope here, got:\n{serialized}"
        );

        // The regression: parsing back must find the keyword. Before the fix,
        // dc:subject silently used an unbound prefix, so the write round-tripped
        // to zero keywords and labelling a photo private had no effect.
        assert_eq!(
            XmpSidecar::parse(serialized.as_bytes())?.keywords(),
            ["private"]
        );
        Ok(())
    }

    /// An existing subject can be in a later top-level description. Its prefix
    /// scope is independent from the first description, which only serves as
    /// the insertion target when no subject exists yet.
    #[test]
    fn existing_subject_uses_its_own_description_namespace_scope() -> Result<()> {
        const SUBJECT_IN_LATER_DESCRIPTION: &str = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <rdf:Description rdf:about=\"\" xmlns:dcx=\"http://purl.org/dc/elements/1.1/\" dcx:format=\"image/jpeg\"/>\n",
            "  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n",
            "   <dc:subject><rdf:Bag><rdf:li>alpha</rdf:li></rdf:Bag></dc:subject>\n",
            "  </rdf:Description>\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
        );

        let mut sidecar = XmpSidecar::parse(SUBJECT_IN_LATER_DESCRIPTION.as_bytes())?;
        assert_eq!(sidecar.keywords(), ["alpha"]);

        sidecar.set_keywords(vec!["private".to_owned()]);
        let serialized = serialize(&sidecar)?;

        assert!(
            serialized.contains("<dc:subject>") && !serialized.contains("<dcx:subject>"),
            "the regenerated subject must use the prefix in scope at its target, got:\n{serialized}"
        );
        assert_eq!(
            XmpSidecar::parse(serialized.as_bytes())?.keywords(),
            ["private"]
        );
        Ok(())
    }

    #[test]
    fn packet_without_rdf_description_gets_one() -> Result<()> {
        const WITHOUT_DESCRIPTION: &str = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
        );

        let mut sidecar = XmpSidecar::parse(WITHOUT_DESCRIPTION.as_bytes())?;
        sidecar.set_keywords(vec!["private".to_owned()]);
        let serialized = serialize(&sidecar)?;

        assert!(
            serialized.contains("<rdf:Description rdf:about=\"\""),
            "a description must be created, got:\n{serialized}"
        );
        assert_eq!(
            XmpSidecar::parse(serialized.as_bytes())?.keywords(),
            ["private"]
        );
        Ok(())
    }

    #[test]
    fn packet_without_indentation_stays_compact() -> Result<()> {
        const COMPACT: &str = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"/>",
            "</rdf:RDF>",
            "</x:xmpmeta>",
        );

        let mut sidecar = XmpSidecar::parse(COMPACT.as_bytes())?;
        sidecar.set_keywords(vec!["private".to_owned()]);
        let serialized = serialize(&sidecar)?;

        assert!(
            !serialized.contains('\n'),
            "a packet without line breaks must not gain any, got:\n{serialized}"
        );
        assert!(
            serialized
                .contains("<dc:subject><rdf:Bag><rdf:li>private</rdf:li></rdf:Bag></dc:subject>"),
            "unexpected serialization:\n{serialized}"
        );
        assert_eq!(
            XmpSidecar::parse(serialized.as_bytes())?.keywords(),
            ["private"]
        );
        Ok(())
    }

    #[test]
    fn keywords_with_markup_characters_survive_a_round_trip() -> Result<()> {
        let mut sidecar = XmpSidecar::new_empty();
        let keywords = vec![
            "rock & roll".to_owned(),
            "<tag>".to_owned(),
            "quote\"and'apostrophe".to_owned(),
            "umlaut Ä".to_owned(),
        ];
        sidecar.set_keywords(keywords.clone());

        let reparsed = XmpSidecar::parse(&sidecar.to_bytes()?)?;
        assert_eq!(reparsed.keywords(), keywords.as_slice());
        Ok(())
    }

    #[test]
    fn parse_rejects_input_without_rdf_root() {
        let error = XmpSidecar::parse(b"<html><body>not xmp</body></html>")
            .expect_err("non-XMP input must be rejected");
        assert!(
            error.to_string().contains("rdf:RDF"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn media_and_sidecar_keys_round_trip() {
        for media_key in ["photo.jpg", "album/photo.jpg", "a/b/c/photo.png", "no-ext"] {
            let sidecar_key = sidecar_key_for_media(media_key);
            assert_eq!(sidecar_key, format!("{media_key}.xmp"));
            assert!(is_sidecar_key(&sidecar_key));
            assert_eq!(
                media_key_for_sidecar(&sidecar_key).as_deref(),
                Some(media_key)
            );
        }
    }

    #[test]
    fn non_sidecar_keys_are_not_recognized() {
        for key in [
            "photo.jpg",
            "album/photo.jpg",
            ".xmp",
            "album/.xmp",
            "album\\.xmp",
            "photo.xmp.jpg",
            "",
            "photo.XMP",
        ] {
            assert!(!is_sidecar_key(key), "{key:?} must not be a sidecar key");
            assert_eq!(media_key_for_sidecar(key), None, "{key:?}");
        }
    }
}

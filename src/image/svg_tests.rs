//! The svg of the image tests of spython, one test for each file in its
//! `cli/tests/images` that prints an svg, with the same name. `rhombus3`
//! comes from the tests of sgleam, since spython refuses its angle.

use super::*;
use crate::scene::{FontStyle, LineCap, LineJoin};
use insta::assert_snapshot;

/// The svg of `img`, as `to_svg` of spython prints it. Spython runs each
/// test in a process of its own, so the number of each svg is 0.
fn svg(img: &Image) -> String {
    img.to_svg(0).unwrap()
}

fn len(v: f64) -> Length {
    Length::new(v).unwrap()
}

fn fin(v: f64) -> Finite {
    Finite::new(v).unwrap()
}

fn pt(x: f64, y: f64) -> Point {
    Point::new(fin(x), fin(y))
}

fn pts(points: &[(f64, f64)]) -> Vec<Point> {
    points.iter().map(|&(x, y)| pt(x, y)).collect()
}

fn c(name: &str) -> Color {
    Color::named(name).unwrap()
}

fn rgba(r: u8, g: u8, b: u8, a: f64) -> Color {
    Color::rgba(r, g, b, Opacity::new(a).unwrap())
}

fn fill(color: Color) -> Style {
    Style::new([Attribute::Fill(color)])
}

fn fill_opacity(color: Color, opacity: f64) -> Style {
    Style::new([
        Attribute::Fill(color),
        Attribute::FillOpacity(Opacity::new(opacity).unwrap()),
    ])
}

fn stroke(color: Color) -> Style {
    Style::new([Attribute::Stroke(color)])
}

fn stroke_width(color: Color, width: f64) -> Style {
    Style::new([Attribute::Stroke(color), Attribute::StrokeWidth(len(width))])
}

fn dash(values: &[f64]) -> Attribute {
    Attribute::StrokeDashArray(values.iter().map(|&v| len(v)).collect())
}

fn round_stroke(color: Color, width: f64) -> Style {
    Style::new([
        Attribute::Stroke(color),
        Attribute::StrokeWidth(len(width)),
        Attribute::StrokeLineCap(LineCap::Round),
        Attribute::StrokeLineJoin(LineJoin::Round),
    ])
}

fn font(size: f64) -> Font {
    Font {
        size: len(size),
        ..Font::default()
    }
}

fn rectangle(w: f64, h: f64, style: Style) -> Image {
    Image::rectangle(len(w), len(h), style)
}

fn square(side: f64, style: Style) -> Image {
    Image::square(len(side), style)
}

fn ellipse(w: f64, h: f64, style: Style) -> Image {
    Image::ellipse(len(w), len(h), style).unwrap()
}

fn circle(radius: f64, style: Style) -> Image {
    Image::circle(len(radius), style).unwrap()
}

fn line(x: f64, y: f64, style: Style) -> Image {
    Image::line(fin(x), fin(y), style)
}

fn triangle(side: f64, style: Style) -> Image {
    Image::triangle(len(side), style)
}

fn star_polygon(side: f64, side_count: u32, step_count: u32, style: Style) -> Image {
    let (n, step) = (SideCount::new(side_count), StepCount::new(step_count));
    Image::star_polygon(len(side), n.unwrap(), step.unwrap(), style).unwrap()
}

fn regular_polygon(side: f64, side_count: u32, style: Style) -> Image {
    Image::regular_polygon(len(side), SideCount::new(side_count).unwrap(), style).unwrap()
}

fn rhombus(side: f64, angle: f64, style: Style) -> Image {
    Image::rhombus(len(side), fin(angle), style).unwrap()
}

fn text(text: &str, style: Style, font: Font) -> Image {
    Image::text(text, font, style).unwrap()
}

fn bitmap(uri: &str) -> Image {
    Image::bitmap_data_uri(uri).unwrap()
}

fn rotate(img: &Image, angle: f64) -> Image {
    img.rotate(fin(angle)).unwrap()
}

fn scale(img: &Image, factor: f64) -> Image {
    img.scale(Factor::new(factor).unwrap()).unwrap()
}

fn scale_xy(img: &Image, x: f64, y: f64) -> Image {
    let (x, y) = (Factor::new(x).unwrap(), Factor::new(y).unwrap());
    img.scale_xy(x, y).unwrap()
}

fn frame(img: &Image) -> Image {
    img.frame(c("black"))
}

fn crop(img: &Image, x: f64, y: f64, w: f64, h: f64) -> Image {
    img.crop(fin(x), fin(y), len(w), len(h))
}

fn crop_align(img: &Image, x_place: XPlace, y_place: YPlace, w: f64, h: f64) -> Image {
    img.crop_align(x_place, y_place, len(w), len(h))
}

fn above(x_place: XPlace, images: impl IntoIterator<Item = Image>) -> Image {
    Image::above(x_place, images).unwrap()
}

fn beside(y_place: YPlace, images: impl IntoIterator<Item = Image>) -> Image {
    Image::beside(y_place, images).unwrap()
}

fn overlay(
    x_place: XPlace,
    y_place: YPlace,
    (x, y): (f64, f64),
    images: impl IntoIterator<Item = Image>,
) -> Image {
    Image::overlay(x_place, y_place, pt(x, y), images).unwrap()
}

fn underlay(
    x_place: XPlace,
    y_place: YPlace,
    (x, y): (f64, f64),
    images: impl IntoIterator<Item = Image>,
) -> Image {
    Image::underlay(x_place, y_place, pt(x, y), images).unwrap()
}

fn overlay_xy(top: &Image, x: f64, y: f64, bottom: &Image) -> Image {
    top.overlay_xy(pt(x, y), bottom).unwrap()
}

fn underlay_xy(bottom: &Image, x: f64, y: f64, top: &Image) -> Image {
    bottom.underlay_xy(pt(x, y), top).unwrap()
}

fn empty_scene(w: f64, h: f64) -> Image {
    Image::empty_scene(len(w), len(h), c("black"))
}

fn place_image(scene: &Image, x: f64, y: f64, img: &Image) -> Image {
    place_image_align(scene, x, y, img, XPlace::Center, YPlace::Middle)
}

fn place_image_align(
    scene: &Image,
    x: f64,
    y: f64,
    img: &Image,
    x_place: XPlace,
    y_place: YPlace,
) -> Image {
    scene
        .place_image(fin(x), fin(y), x_place, y_place, img)
        .unwrap()
}

fn place_images(
    scene: &Image,
    points: &[(f64, f64)],
    images: impl IntoIterator<Item = Image>,
    x_place: XPlace,
    y_place: YPlace,
) -> Image {
    let items = pts(points).into_iter().zip(images);
    scene.place_images(x_place, y_place, items).unwrap()
}

/// The ends of a curve from `(x1, y1)` to `(x2, y2)`, each with its angle
/// and its pull.
fn ends([x1, y1, a1, p1, x2, y2, a2, p2]: [f64; 8]) -> (CurveEnd, CurveEnd) {
    let end = |x, y, angle, pull| CurveEnd {
        point: pt(x, y),
        angle: fin(angle),
        pull: fin(pull),
    };
    (end(x1, y1, a1, p1), end(x2, y2, a2, p2))
}

use XPlace::{Center, Left, Right};
use YPlace::{Bottom, Middle, Top};

#[test]
fn above1() {
    let img = above(
        Center,
        [
            above(
                Center,
                [
                    above(
                        Center,
                        [
                            ellipse(70.0, 20.0, fill(c("lightgray"))),
                            ellipse(50.0, 20.0, fill(c("darkgray"))),
                        ],
                    ),
                    ellipse(30.0, 20.0, fill(c("dimgray"))),
                ],
            ),
            ellipse(10.0, 20.0, fill(c("black"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn above_align1() {
    let img = above(
        Right,
        [
            ellipse(70.0, 20.0, fill(c("yellowgreen"))),
            ellipse(50.0, 20.0, fill(c("olivedrab"))),
            ellipse(30.0, 20.0, fill(c("darkolivegreen"))),
            ellipse(10.0, 20.0, fill(c("darkgreen"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn above_align2() {
    let img = above(
        Left,
        [
            ellipse(70.0, 20.0, fill(c("gold"))),
            ellipse(50.0, 20.0, fill(c("goldenrod"))),
            ellipse(30.0, 20.0, fill(c("darkgoldenrod"))),
            ellipse(10.0, 20.0, fill(c("sienna"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn above_variadic1() {
    let img = above(
        Left,
        [
            ellipse(60.0, 16.0, fill(c("navy"))),
            ellipse(40.0, 16.0, fill(c("blue"))),
            ellipse(20.0, 16.0, fill(c("dodgerblue"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn add_curve1() {
    let (from, to) = ends([20.0, 20.0, 0.0, 0.333, 80.0, 80.0, 0.0, 0.333]);
    let img = rectangle(100.0, 100.0, stroke(c("black")))
        .add_curve(from, to, stroke(c("red")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_curve2() {
    let (from, to) = ends([50.0, 10.0, 270.0, 0.5, 50.0, 90.0, 90.0, 0.5]);
    let img = rectangle(100.0, 100.0, stroke(c("black")))
        .add_curve(from, to, stroke(c("red")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_curve3() {
    let (from, to) = ends([20.0, 50.0, 0.0, 0.0, 80.0, 50.0, 0.0, 0.0]);
    let img = rectangle(100.0, 100.0, stroke(c("black")))
        .add_curve(from, to, stroke(c("red")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_line1() {
    let img = ellipse(40.0, 40.0, stroke(c("maroon")))
        .add_line(pt(0.0, 40.0), pt(40.0, 0.0), stroke(c("maroon")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_line2() {
    let img = rectangle(40.0, 40.0, fill(c("gray")))
        .add_line(pt(-10.0, 50.0), pt(50.0, -10.0), stroke(c("maroon")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_line3() {
    let img = rectangle(100.0, 100.0, fill(c("darkolivegreen")))
        .add_line(
            pt(25.0, 25.0),
            pt(75.0, 75.0),
            round_stroke(c("goldenrod"), 30.0),
        )
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_polygon1() {
    let points = pts(&[(30.0, -20.0), (50.0, 50.0), (-20.0, 30.0)]);
    let img = square(65.0, fill(c("lightblue")))
        .add_polygon(&points, fill(c("forestgreen")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_polygon2() {
    let points = pts(&[
        (109.0, 160.0),
        (26.0, 148.0),
        (46.0, 36.0),
        (93.0, 44.0),
        (89.0, 68.0),
        (122.0, 72.0),
    ]);
    let img = square(180.0, fill(c("yellow")))
        .add_polygon(&points, stroke(c("darkblue")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_polygon3() {
    let points = pts(&[(25.0, -10.0), (60.0, 25.0), (25.0, 60.0), (-10.0, 25.0)]);
    let img = square(50.0, fill(c("lightblue")))
        .add_polygon(&points, fill(c("pink")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_solid_curve1() {
    let (from, to) = ends([20.0, 20.0, 0.0, 0.333, 80.0, 80.0, 0.0, 0.333]);
    let img = rectangle(100.0, 100.0, stroke(c("black")))
        .add_solid_curve(from, to, fill(c("red")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn add_wedge1() {
    let angle = WedgeAngle::new(90.0).unwrap();
    let img = circle(40.0, stroke(c("black")))
        .add_wedge(pt(40.0, 40.0), len(40.0), angle, fill(c("red")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn beside1() {
    let img = beside(
        Middle,
        [
            beside(
                Middle,
                [
                    beside(
                        Middle,
                        [
                            ellipse(20.0, 70.0, fill(c("lightgray"))),
                            ellipse(20.0, 50.0, fill(c("darkgray"))),
                        ],
                    ),
                    ellipse(20.0, 30.0, fill(c("dimgray"))),
                ],
            ),
            ellipse(20.0, 10.0, fill(c("black"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn beside_align1() {
    let img = beside(
        Bottom,
        [
            ellipse(20.0, 70.0, fill(c("lightsteelblue"))),
            ellipse(20.0, 50.0, fill(c("mediumslateblue"))),
            ellipse(20.0, 30.0, fill(c("slateblue"))),
            ellipse(20.0, 10.0, fill(c("navy"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn beside_align2() {
    let img = beside(
        Top,
        [
            ellipse(20.0, 70.0, fill(c("mediumorchid"))),
            ellipse(20.0, 50.0, fill(c("darkorchid"))),
            ellipse(20.0, 30.0, fill(c("purple"))),
            ellipse(20.0, 10.0, fill(c("indigo"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn beside_variadic1() {
    let img = beside(
        Top,
        [
            ellipse(16.0, 60.0, fill(c("navy"))),
            ellipse(16.0, 40.0, fill(c("blue"))),
            ellipse(16.0, 20.0, fill(c("dodgerblue"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn bitmap1() {
    let img = bitmap(
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==",
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn bitmap2() {
    let img = bitmap(
        "data:image/bmp;base64,Qk06AAAAAAAAADYAAAAoAAAAAQAAAAEAAAABABgAAAAAAAQAAAATCwAAEwsAAAAAAAAAAAAAAAD/AA==",
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn circle1() {
    assert_snapshot!(svg(&circle(30.0, stroke(c("red")))));
}

#[test]
fn circle2() {
    assert_snapshot!(svg(&circle(20.0, fill(c("blue")))));
}

#[test]
fn crop1() {
    let img = crop(&circle(40.0, fill(c("chocolate"))), 0.0, 0.0, 40.0, 40.0);
    assert_snapshot!(svg(&img));
}

#[test]
fn crop2() {
    let img = crop(
        &ellipse(80.0, 120.0, fill(c("dodgerblue"))),
        40.0,
        60.0,
        40.0,
        60.0,
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn crop3() {
    let quarter = |color, x, y| crop(&circle(40.0, fill(c(color))), x, y, 40.0, 40.0);
    let img = above(
        Center,
        [
            beside(
                Middle,
                [
                    quarter("palevioletred", 40.0, 40.0),
                    quarter("lightcoral", 0.0, 40.0),
                ],
            ),
            beside(
                Middle,
                [
                    quarter("lightcoral", 40.0, 0.0),
                    quarter("palevioletred", 0.0, 0.0),
                ],
            ),
        ],
    );
    assert_snapshot!(svg(&rotate(&img, 30.0)));
}

#[test]
fn crop_align1() {
    let img = crop_align(&circle(40.0, fill(c("chocolate"))), Left, Top, 40.0, 40.0);
    assert_snapshot!(svg(&img));
}

#[test]
fn crop_align2() {
    let img = ellipse(80.0, 120.0, fill(c("dodgerblue")));
    assert_snapshot!(svg(&crop_align(&img, Right, Bottom, 40.0, 60.0)));
}

#[test]
fn crop_align3() {
    let img = circle(25.0, fill(c("mediumslateblue")));
    assert_snapshot!(svg(&crop_align(&img, Center, Middle, 50.0, 30.0)));
}

#[test]
fn crop_align4() {
    let quarter = |color, x_place, y_place| {
        crop_align(&circle(40.0, fill(c(color))), x_place, y_place, 40.0, 40.0)
    };
    let img = above(
        Center,
        [
            beside(
                Middle,
                [
                    quarter("palevioletred", Right, Bottom),
                    quarter("lightcoral", Left, Bottom),
                ],
            ),
            beside(
                Middle,
                [
                    quarter("lightcoral", Right, Top),
                    quarter("palevioletred", Left, Top),
                ],
            ),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn crop_align5() {
    let square = rectangle(10.0, 10.0, fill(c("red")));
    let img = beside(
        Middle,
        [
            frame(&crop_align(&square, Left, Top, 20.0, 20.0)),
            frame(&crop_align(&square, Center, Middle, 20.0, 20.0)),
            frame(&crop_align(&square, Right, Bottom, 20.0, 20.0)),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn dash1() {
    let style = Style::new([Attribute::Stroke(c("red")), dash(&[6.0, 3.0])]);
    assert_snapshot!(svg(&circle(30.0, style)));
}

#[test]
fn dash2() {
    let style = Style::new([Attribute::Stroke(c("blue")), dash(&[2.0, 2.0])]);
    assert_snapshot!(svg(&rectangle(80.0, 40.0, style)));
}

#[test]
fn dash3() {
    let style = Style::new([
        Attribute::Stroke(c("black")),
        Attribute::StrokeWidth(len(2.0)),
        dash(&[6.0, 3.0, 2.0, 3.0]),
    ]);
    assert_snapshot!(svg(&line(100.0, 0.0, style)));
}

#[test]
fn dash4() {
    let style = Style::new([Attribute::Stroke(c("black")), dash(&[2.5, 1.5])]);
    assert_snapshot!(svg(&rectangle(80.0, 40.0, style)));
}

#[test]
fn ellipse1() {
    assert_snapshot!(svg(&ellipse(60.0, 30.0, stroke(c("black")))));
}

#[test]
fn ellipse2() {
    assert_snapshot!(svg(&ellipse(30.0, 60.0, fill(c("blue")))));
}

#[test]
fn ellipse3() {
    let img = beside(
        Middle,
        [
            ellipse(0.0, 20.0, stroke(c("black"))),
            ellipse(20.0, 0.0, stroke(c("black"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn empty_scene1() {
    assert_snapshot!(svg(&empty_scene(160.0, 90.0)));
}

#[test]
fn flip_bitmap1() {
    let pixels = bitmap(
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGP4z8DA8J+BkYHh////DAAe9gT9Ce00PgAAAABJRU5ErkJggg==",
    );
    let img = beside(
        Middle,
        [
            pixels.clone(),
            pixels.flip_horizontal().unwrap(),
            pixels.flip_vertical().unwrap(),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn flip_horizontal1() {
    let img = beside(
        Middle,
        [
            rotate(&square(50.0, fill(c("red"))), 30.0),
            rotate(&square(50.0, fill(c("blue"))), 30.0)
                .flip_horizontal()
                .unwrap(),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn flip_vertical1() {
    let star = |color| Image::star(len(40.0), fill(c(color))).unwrap();
    let flipped = star("gray").flip_vertical().unwrap();
    let img = above(Center, [star("firebrick"), scale_xy(&flipped, 1.0, 0.5)]);
    assert_snapshot!(svg(&img));
}

#[test]
fn frame1() {
    assert_snapshot!(svg(&frame(&ellipse(40.0, 40.0, fill(c("gray"))))));
}

#[test]
fn frame2() {
    let img = beside(
        Middle,
        [
            beside(
                Middle,
                [
                    beside(
                        Middle,
                        [
                            ellipse(20.0, 70.0, fill(c("lightsteelblue"))),
                            frame(&ellipse(20.0, 50.0, fill(c("mediumslateblue")))),
                        ],
                    ),
                    ellipse(20.0, 30.0, fill(c("slateblue"))),
                ],
            ),
            ellipse(20.0, 10.0, fill(c("navy"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

fn isosceles_triangle(side: f64, angle: f64, style: Style) -> Image {
    Image::isosceles_triangle(len(side), fin(angle), style).unwrap()
}

#[test]
fn isosceles_triangle1() {
    assert_snapshot!(svg(&isosceles_triangle(200.0, 170.0, fill(c("seagreen")))));
}

#[test]
fn isosceles_triangle2() {
    assert_snapshot!(svg(&isosceles_triangle(60.0, 30.0, fill(c("aquamarine")))));
}

#[test]
fn isosceles_triangle3() {
    let img = isosceles_triangle(60.0, 330.0, fill(c("lightseagreen")));
    assert_snapshot!(svg(&img));
}

fn koch(n: u32) -> Image {
    if n == 0 {
        return square(1.0, fill(c("black")));
    }
    let smaller = koch(n - 1);
    beside(
        Bottom,
        [
            smaller.clone(),
            rotate(&smaller, 60.0),
            rotate(&smaller, -60.0),
            smaller,
        ],
    )
}

#[test]
fn koch_curve() {
    assert_snapshot!(svg(&koch(5)));
}

#[test]
fn line1() {
    assert_snapshot!(svg(&line(30.0, 30.0, stroke(c("black")))));
}

#[test]
fn line2() {
    assert_snapshot!(svg(&line(-30.0, 20.0, stroke(c("red")))));
}

#[test]
fn line3() {
    assert_snapshot!(svg(&line(30.0, -20.0, stroke(c("red")))));
}

#[test]
fn line4() {
    let img = beside(
        Middle,
        [
            line(0.0, 20.0, stroke(c("black"))),
            line(20.0, 0.0, stroke(c("red"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn line5() {
    let l = line(6.0, 0.0, stroke(c("black")));
    let images = [
        rotate(&l, 45.0),
        rotate(&l, -45.0),
        rotate(&l, 30.0),
        rotate(&line(0.0, 0.0, stroke(c("black"))), 45.0),
        scale(&l, 4.0),
        scale(&l, 0.5),
        scale(&line(6.0, 4.0, stroke(c("black"))), 2.0)
            .flip_horizontal()
            .unwrap(),
    ];
    let svgs: Vec<_> = images.iter().map(svg).collect();
    assert_snapshot!(svgs.join("\n"));
}

#[test]
fn overlay1() {
    let img = overlay(
        Center,
        Middle,
        (0.0, 0.0),
        [
            rectangle(30.0, 60.0, fill(c("orange"))),
            ellipse(60.0, 30.0, fill(c("purple"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay2() {
    let pair = |a, b| overlay(Center, Middle, (0.0, 0.0), [a, b]);
    let e = |size, color| ellipse(size, size, fill(c(color)));
    let img = pair(
        pair(
            pair(
                pair(pair(e(10.0, "red"), e(20.0, "black")), e(30.0, "red")),
                e(40.0, "black"),
            ),
            e(50.0, "red"),
        ),
        e(60.0, "black"),
    );
    assert_snapshot!(svg(&img));
}

fn blue_pentagon(side: f64, level: u8) -> Image {
    regular_polygon(side, 5, fill(Color::rgb(level, level, 255)))
}

#[test]
fn overlay3() {
    let pair = |a, b| overlay(Center, Middle, (0.0, 0.0), [a, b]);
    let img = pair(
        pair(
            pair(
                pair(blue_pentagon(20.0, 50), blue_pentagon(26.0, 100)),
                blue_pentagon(32.0, 150),
            ),
            blue_pentagon(38.0, 200),
        ),
        blue_pentagon(44.0, 250),
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_align1() {
    let img = overlay(
        Left,
        Middle,
        (0.0, 0.0),
        [
            rectangle(30.0, 60.0, fill(c("orange"))),
            ellipse(60.0, 30.0, fill(c("purple"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_align2() {
    let img = overlay(
        Right,
        Bottom,
        (0.0, 0.0),
        [
            square(20.0, fill(c("silver"))),
            square(30.0, fill(c("seagreen"))),
            square(40.0, fill(c("silver"))),
            square(50.0, fill(c("seagreen"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

fn star_and_circle() -> [Image; 2] {
    [
        star_polygon(20.0, 20, 3, fill(c("navy"))),
        circle(30.0, fill(c("cornflowerblue"))),
    ]
}

#[test]
fn overlay_align_offset1() {
    let img = overlay(Right, Bottom, (10.0, 10.0), star_and_circle());
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_align_offset2() {
    let img = overlay(Left, Bottom, (-10.0, 10.0), star_and_circle());
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_offset1() {
    let img = overlay(
        Center,
        Middle,
        (10.0, 10.0),
        [circle(40.0, fill(c("red"))), circle(40.0, fill(c("blue")))],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_offset2() {
    let orange = || circle(20.0, fill(c("darkorange")));
    let inner = overlay(
        Center,
        Middle,
        (-50.0, 0.0),
        [rectangle(60.0, 20.0, fill(c("black"))), orange()],
    );
    let img = overlay(Center, Middle, (70.0, 0.0), [inner, orange()]);
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_offset3() {
    let inner = overlay(
        Center,
        Middle,
        (26.0, 0.0),
        [
            circle(30.0, fill(rgba(0, 150, 0, 0.5))),
            circle(30.0, fill(rgba(0, 0, 255, 0.5))),
        ],
    );
    let img = overlay(
        Center,
        Middle,
        (0.0, 26.0),
        [inner, circle(30.0, fill(rgba(200, 0, 0, 0.5)))],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_variadic1() {
    let img = overlay(
        Center,
        Middle,
        (0.0, 0.0),
        [
            blue_pentagon(20.0, 50),
            blue_pentagon(26.0, 100),
            blue_pentagon(32.0, 150),
            blue_pentagon(38.0, 200),
            blue_pentagon(44.0, 250),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn overlay_xy1() {
    let r = || rectangle(20.0, 20.0, stroke(c("black")));
    assert_snapshot!(svg(&overlay_xy(&r(), 20.0, 0.0, &r())));
}

#[test]
fn overlay_xy2() {
    let (red, black) = (
        rectangle(20.0, 20.0, fill(c("red"))),
        rectangle(20.0, 20.0, fill(c("black"))),
    );
    assert_snapshot!(svg(&overlay_xy(&red, 10.0, 10.0, &black)));
}

#[test]
fn overlay_xy3() {
    let (red, black) = (
        rectangle(20.0, 20.0, fill(c("red"))),
        rectangle(20.0, 20.0, fill(c("black"))),
    );
    assert_snapshot!(svg(&overlay_xy(&red, -10.0, -10.0, &black)));
}

#[test]
fn overlay_xy4() {
    let dot = || ellipse(10.0, 10.0, fill(c("forestgreen")));
    let inner = overlay_xy(&ellipse(40.0, 40.0, stroke(c("black"))), 10.0, 15.0, &dot());
    assert_snapshot!(svg(&overlay_xy(&inner, 20.0, 15.0, &dot())));
}

#[test]
fn place_curve1() {
    let (from, to) = ends([10.0, 50.0, 90.0, 0.5, 90.0, 50.0, 90.0, 0.5]);
    let img = empty_scene(100.0, 100.0)
        .place_curve(from, to, stroke(c("blue")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn place_image1() {
    let img = place_image(
        &rectangle(48.0, 48.0, fill(c("lightgray"))),
        24.0,
        24.0,
        &triangle(32.0, fill(c("red"))),
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn place_image2() {
    let img = place_image(
        &rectangle(48.0, 48.0, fill(c("lightgray"))),
        24.0,
        24.0,
        &triangle(64.0, fill(c("red"))),
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn place_image3() {
    let dot = circle(4.0, fill(c("white")));
    let mut img = rectangle(24.0, 24.0, fill(c("goldenrod")));
    for (x, y) in [(8.0, 14.0), (14.0, 2.0), (0.0, 6.0), (18.0, 20.0)] {
        img = place_image(&img, x, y, &dot);
    }
    assert_snapshot!(svg(&img));
}

#[test]
fn place_image4() {
    let mut scene = empty_scene(84.0, 12.0);
    for i in 0..20 {
        let x = 4.0 + f64::from(i) * 4.0;
        scene = place_image(&scene, x, 6.0, &circle(2.0, fill(c("red"))));
    }
    assert_snapshot!(svg(&scene));
}

#[test]
fn place_image_align1() {
    let img = place_image_align(
        &rectangle(64.0, 64.0, fill(c("palegoldenrod"))),
        64.0,
        64.0,
        &triangle(48.0, fill(c("yellowgreen"))),
        Right,
        Bottom,
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn place_image_align2() {
    let img = place_image_align(
        &rectangle(64.0, 64.0, fill(c("palegoldenrod"))),
        0.0,
        0.0,
        &rotate(&triangle(48.0, fill(c("yellowgreen"))), 180.0),
        Left,
        Top,
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn place_image_align3() {
    let at = |v| {
        place_image(
            &rectangle(32.0, 32.0, stroke(c("black"))),
            v,
            v,
            &circle(8.0, fill(c("tomato"))),
        )
    };
    let img = beside(Middle, [at(0.0), at(8.0), at(16.0), at(24.0), at(32.0)]);
    assert_snapshot!(svg(&img));
}

#[test]
fn place_images1() {
    let dot = || circle(4.0, fill(c("white")));
    let img = place_images(
        &rectangle(24.0, 24.0, fill(c("goldenrod"))),
        &[(18.0, 20.0), (0.0, 6.0), (14.0, 2.0)],
        [dot(), dot(), dot()],
        Center,
        Middle,
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn place_images_align1() {
    let t = || triangle(48.0, fill(c("yellowgreen")));
    let img = place_images(
        &rectangle(64.0, 64.0, fill(c("goldenrod"))),
        &[(64.0, 64.0), (64.0, 48.0), (64.0, 32.0)],
        [t(), t(), t()],
        Right,
        Bottom,
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn place_line1() {
    let img = ellipse(40.0, 40.0, stroke(c("maroon")))
        .place_line(pt(0.0, 40.0), pt(40.0, 0.0), stroke(c("maroon")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn place_line2() {
    let img = rectangle(40.0, 40.0, fill(c("lightgray")))
        .place_line(pt(-10.0, 50.0), pt(50.0, -10.0), stroke(c("maroon")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn place_line3() {
    let img = rectangle(100.0, 100.0, fill(c("darkolivegreen")))
        .place_line(
            pt(25.0, 25.0),
            pt(100.0, 100.0),
            round_stroke(c("goldenrod"), 30.0),
        )
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn place_polygon1() {
    let points = pts(&[(30.0, -20.0), (50.0, 50.0), (-20.0, 30.0)]);
    let img = square(65.0, fill(c("lightblue")))
        .place_polygon(&points, fill(c("forestgreen")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn place_polygon2() {
    let points = pts(&[
        (109.0, 160.0),
        (26.0, 148.0),
        (46.0, 36.0),
        (93.0, 44.0),
        (89.0, 68.0),
        (122.0, 72.0),
    ]);
    let img = square(180.0, fill(c("yellow")))
        .place_polygon(&points, stroke(c("darkblue")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

#[test]
fn place_polygon3() {
    let points = pts(&[(25.0, -10.0), (60.0, 25.0), (25.0, 60.0), (-10.0, 25.0)]);
    let img = square(50.0, fill(c("lightblue")))
        .place_polygon(&points, fill(c("pink")))
        .unwrap();
    assert_snapshot!(svg(&img));
}

fn polygon(points: &[(f64, f64)], style: Style) -> Image {
    Image::polygon(&pts(points), style).unwrap()
}

#[test]
fn polygon1() {
    let points = [(0.0, 0.0), (-10.0, 20.0), (60.0, 0.0), (-10.0, -20.0)];
    assert_snapshot!(svg(&polygon(&points, fill(c("burlywood")))));
}

#[test]
fn polygon3() {
    let points = [
        (0.0, 0.0),
        (0.0, 40.0),
        (20.0, 40.0),
        (20.0, 60.0),
        (40.0, 60.0),
        (40.0, 20.0),
        (20.0, 20.0),
        (20.0, 0.0),
    ];
    assert_snapshot!(svg(&polygon(&points, fill(c("plum")))));
}

fn crossed_polygon(cap: LineCap, join: LineJoin) -> Image {
    let style = Style::new([
        Attribute::Stroke(c("darkslategray")),
        Attribute::StrokeWidth(len(10.0)),
        Attribute::StrokeLineCap(cap),
        Attribute::StrokeLineJoin(join),
    ]);
    let points = [(0.0, 0.0), (50.0, 0.0), (0.0, 50.0), (50.0, 50.0)];
    underlay(
        Center,
        Middle,
        (0.0, 0.0),
        [
            rectangle(80.0, 80.0, fill(c("mediumseagreen"))),
            polygon(&points, style),
        ],
    )
}

#[test]
fn polygon4() {
    assert_snapshot!(svg(&crossed_polygon(LineCap::Round, LineJoin::Round)));
}

#[test]
fn polygon5() {
    assert_snapshot!(svg(&crossed_polygon(LineCap::Square, LineJoin::default())));
}

fn pulled_regular_polygon(side: f64, n: u32, pull: f64, angle: f64, style: Style) -> Image {
    let n = SideCount::new(n).unwrap();
    Image::pulled_regular_polygon(len(side), n, fin(pull), fin(angle), style).unwrap()
}

#[test]
fn pulled_regular_polygon1() {
    let img = pulled_regular_polygon(50.0, 5, 0.333, 30.0, fill(c("green")));
    assert_snapshot!(svg(&img));
}

#[test]
fn pulled_regular_polygon2() {
    let img = pulled_regular_polygon(50.0, 6, 0.5, 45.0, stroke(c("red")));
    assert_snapshot!(svg(&img));
}

#[test]
fn pulled_regular_polygon3() {
    let img = pulled_regular_polygon(50.0, 4, 0.0, 0.0, fill(c("orange")));
    assert_snapshot!(svg(&img));
}

#[test]
fn put_image1() {
    let img = rectangle(50.0, 50.0, fill(c("lightgray")))
        .put_image(fin(40.0), fin(15.0), &ellipse(20.0, 30.0, fill(c("red"))))
        .unwrap();
    assert_snapshot!(svg(&img));
}

fn radial_star(n: u32, inner: f64, outer: f64, style: Style) -> Image {
    let n = PointCount::new(n).unwrap();
    Image::radial_star(n, len(inner), len(outer), style).unwrap()
}

#[test]
fn radial_star1() {
    assert_snapshot!(svg(&radial_star(8, 8.0, 64.0, fill(c("darkslategray")))));
}

#[test]
fn radial_star2() {
    assert_snapshot!(svg(&radial_star(32, 30.0, 40.0, stroke(c("black")))));
}

#[test]
fn rectangle1() {
    assert_snapshot!(svg(&rectangle(40.0, 20.0, stroke(c("black")))));
}

#[test]
fn rectangle2() {
    assert_snapshot!(svg(&rectangle(20.0, 40.0, fill(c("black")))));
}

#[test]
fn regular_polygon1() {
    assert_snapshot!(svg(&regular_polygon(50.0, 3, stroke(c("red")))));
}

#[test]
fn regular_polygon2() {
    assert_snapshot!(svg(&regular_polygon(40.0, 4, fill(c("blue")))));
}

#[test]
fn regular_polygon3() {
    assert_snapshot!(svg(&regular_polygon(20.0, 8, fill(c("red")))));
}

#[test]
fn rhombus1() {
    assert_snapshot!(svg(&rhombus(40.0, 45.0, fill(c("magenta")))));
}

#[test]
fn rhombus2() {
    assert_snapshot!(svg(&rhombus(80.0, 150.0, fill(c("mediumpurple")))));
}

#[test]
fn rhombus3() {
    assert_snapshot!(svg(&rhombus(40.0, 200.0, fill(c("mediumpurple")))));
}

#[test]
fn right_triangle1() {
    let img = Image::right_triangle(len(36.0), len(48.0), fill(c("black")));
    assert_snapshot!(svg(&img));
}

#[test]
fn rotate1() {
    let img = ellipse(60.0, 20.0, fill(c("olivedrab")));
    assert_snapshot!(svg(&rotate(&img, 45.0)));
}

#[test]
fn rotate2() {
    let img = rectangle(50.0, 50.0, stroke(c("black")));
    assert_snapshot!(svg(&rotate(&img, 5.0)));
}

#[test]
fn rotate3() {
    let img = beside(
        Middle,
        [
            rectangle(40.0, 20.0, fill(c("darkseagreen"))),
            rectangle(20.0, 100.0, fill(c("darkseagreen"))),
        ],
    );
    assert_snapshot!(svg(&rotate(&img, 45.0)));
}

#[test]
fn rotate_combine1() {
    let img = beside(Middle, [rectangle(40.0, 20.0, fill(c("red")))]);
    assert_snapshot!(svg(&rotate(&img, 90.0)));
}

#[test]
fn scale1() {
    let img = beside(
        Middle,
        [
            scale(&ellipse(20.0, 30.0, fill(c("blue"))), 2.0),
            ellipse(40.0, 60.0, fill(c("blue"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn scale2() {
    let img = beside(
        Middle,
        [
            rectangle(100.0, 200.0, fill(c("black"))),
            rectangle(200.0, 100.0, fill(c("blue"))),
        ],
    );
    assert_snapshot!(svg(&scale(&img, 2.0)));
}

#[test]
fn scale_stroke1() {
    let thick = rectangle(20.0, 20.0, stroke_width(c("black"), 2.0));
    let thin = rectangle(20.0, 20.0, stroke(c("black")));
    let img = beside(
        Middle,
        [
            thick.clone(),
            scale(&thick, 3.0),
            scale_xy(&thick, 4.0, 1.0),
            scale(&thin, 4.0),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn scale_stroke2() {
    let style = Style::new([
        Attribute::Stroke(c("black")),
        Attribute::StrokeWidth(len(2.0)),
        dash(&[4.0, 2.0]),
    ]);
    let dashed = rectangle(20.0, 20.0, style);
    let img = beside(
        Middle,
        [
            dashed.clone(),
            scale(&dashed, 3.0),
            scale_xy(&dashed, 4.0, 1.0),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn scale_stroke3() {
    let thick = text("Hi", stroke_width(c("black"), 2.0), font(20.0));
    let thin = text("Hi", stroke(c("black")), font(20.0));
    let img = beside(
        Middle,
        [
            thick.clone(),
            scale(&thick, 3.0),
            scale_xy(&thick, 4.0, 1.0),
            scale(&thin, 4.0),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn scale_xy1() {
    let img = beside(
        Middle,
        [
            scale_xy(&ellipse(20.0, 30.0, fill(c("blue"))), 3.0, 2.0),
            ellipse(60.0, 60.0, fill(c("blue"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn scale_xy_rotated1() {
    let e = rotate(&ellipse(40.0, 10.0, fill(c("blue"))), 45.0);
    let img = beside(
        Middle,
        [e.clone(), scale_xy(&e, 2.0, 1.0), scale_xy(&e, 1.0, 3.0)],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn scale_xy_rotated2() {
    let t = rotate(&text("abc", Style::default(), font(20.0)), 45.0);
    let cropped = rotate(
        &crop(
            &rectangle(40.0, 40.0, fill(c("blue"))),
            0.0,
            0.0,
            30.0,
            10.0,
        ),
        45.0,
    );
    let img = beside(
        Middle,
        [
            frame(&t),
            frame(&scale_xy(&t, 3.0, 1.0)),
            frame(&cropped),
            frame(&scale_xy(&cropped, 3.0, 1.0)),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn square1() {
    assert_snapshot!(svg(&square(40.0, fill(c("slateblue")))));
}

#[test]
fn square2() {
    assert_snapshot!(svg(&square(50.0, stroke(c("darkmagenta")))));
}

#[test]
fn star_polygon1() {
    assert_snapshot!(svg(&star_polygon(40.0, 5, 2, fill(c("seagreen")))));
}

#[test]
fn star_polygon2() {
    assert_snapshot!(svg(&star_polygon(40.0, 7, 3, stroke(c("darkred")))));
}

#[test]
fn star_polygon3() {
    assert_snapshot!(svg(&star_polygon(20.0, 10, 3, fill(c("cornflowerblue")))));
}

#[test]
fn stroke_width1() {
    assert_snapshot!(svg(&rectangle(6.0, 4.0, stroke_width(c("black"), 1.0))));
}

#[test]
fn text1() {
    assert_snapshot!(svg(&text("Testing text", fill(c("black")), font(16.0))));
}

#[test]
fn text2() {
    let bold = Font {
        weight: FontWeight::Bold,
        ..font(20.0)
    };
    assert_snapshot!(svg(&text("Bold text", fill(c("red")), bold)));
}

#[test]
fn text3() {
    let italic = Font {
        style: FontStyle::Italic,
        ..font(20.0)
    };
    assert_snapshot!(svg(&text("Italic text", fill(c("blue")), italic)));
}

#[test]
fn text4() {
    let underline = Font {
        underline: true,
        ..font(18.0)
    };
    assert_snapshot!(svg(&text("Underlined", fill(c("green")), underline)));
}

#[test]
fn text5() {
    let all = Font {
        style: FontStyle::Italic,
        weight: FontWeight::Bold,
        underline: true,
        ..font(24.0)
    };
    let img = text("Bold Italic Underline", fill(c("black")), all);
    assert_snapshot!(svg(&img));
}

#[test]
fn text6() {
    let img = overlay(
        Center,
        Middle,
        (0.0, 0.0),
        [
            text("Hello", fill(c("white")), font(20.0)),
            rectangle(80.0, 30.0, fill(c("blue"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn text7() {
    let mono = Font {
        family: "monospace".into(),
        weight: FontWeight::Light,
        ..font(16.0)
    };
    assert_snapshot!(svg(&text("Light mono", fill(c("darkgray")), mono)));
}

#[test]
fn text_empty() {
    let img = beside(
        Middle,
        [
            text("", fill(c("black")), font(16.0)),
            text("a", fill(c("red")), font(16.0)),
            text("", fill(c("black")), font(16.0)),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn triangle1() {
    assert_snapshot!(svg(&triangle(40.0, fill(c("tan")))));
}

fn angle(v: f64) -> TriangleAngle {
    TriangleAngle::new(v).unwrap()
}

#[test]
fn triangle_aas1() {
    let img = Image::triangle_aas(angle(10.0), angle(40.0), len(200.0), fill(c("seagreen")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn triangle_asa1() {
    let img = Image::triangle_asa(angle(10.0), len(200.0), angle(40.0), fill(c("seagreen")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn triangle_ass1() {
    let img = Image::triangle_ass(angle(10.0), len(60.0), len(100.0), fill(c("seagreen")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn triangle_saa1() {
    let img = Image::triangle_saa(len(100.0), angle(10.0), angle(40.0), fill(c("seagreen")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn triangle_sas1() {
    let img = Image::triangle_sas(len(60.0), angle(10.0), len(100.0), fill(c("seagreen")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn triangle_ssa1() {
    let img = Image::triangle_ssa(len(60.0), len(100.0), angle(10.0), fill(c("seagreen")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn triangle_sss1() {
    let img = Image::triangle_sss(len(40.0), len(60.0), len(80.0), fill(c("seagreen")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn triangle_sss2() {
    let img = Image::triangle_sss(len(40.0), len(60.0), len(80.0), stroke(c("black")));
    assert_snapshot!(svg(&img.unwrap()));
}

#[test]
fn underlay1() {
    let img = underlay(
        Center,
        Middle,
        (0.0, 0.0),
        [
            rectangle(30.0, 60.0, fill(c("orange"))),
            ellipse(60.0, 30.0, fill(c("purple"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay2() {
    let e = |w, h, color| ellipse(w, h, fill(c(color)));
    let img = underlay(
        Center,
        Middle,
        (0.0, 0.0),
        [
            e(10.0, 60.0, "red"),
            e(20.0, 50.0, "black"),
            e(30.0, 40.0, "red"),
            e(40.0, 30.0, "black"),
            e(50.0, 20.0, "red"),
            e(60.0, 10.0, "black"),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay3() {
    let e = |w, h| ellipse(w, h, fill_opacity(c("red"), 0.2));
    let img = underlay(
        Center,
        Middle,
        (0.0, 0.0),
        [
            e(10.0, 60.0),
            e(20.0, 50.0),
            e(30.0, 40.0),
            e(40.0, 30.0),
            e(50.0, 20.0),
            e(60.0, 10.0),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_align1() {
    let img = underlay(
        Left,
        Middle,
        (0.0, 0.0),
        [
            rectangle(30.0, 60.0, fill(c("orange"))),
            ellipse(60.0, 30.0, fill(c("purple"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_align2() {
    let img = underlay(
        Right,
        Top,
        (0.0, 0.0),
        [
            square(50.0, fill(c("seagreen"))),
            square(40.0, fill(c("silver"))),
            square(30.0, fill(c("seagreen"))),
            square(20.0, fill(c("silver"))),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_align3() {
    let s = || fill_opacity(c("seagreen"), 0.25);
    let img = underlay(
        Left,
        Middle,
        (0.0, 0.0),
        [
            square(50.0, s()),
            square(40.0, s()),
            square(30.0, s()),
            square(20.0, s()),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_align_offset1() {
    let img = underlay(Right, Bottom, (10.0, 10.0), star_and_circle());
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_align_offset2() {
    let img = underlay(Right, Bottom, (10.0, 10.0), star_and_circle());
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_align_offset3() {
    let star = star_polygon(20.0, 11, 3, fill(c("cornflowerblue")));
    let mut img = rhombus(120.0, 90.0, fill(c("navy")));
    for (x_place, y_place, dx, dy) in [
        (Left, Top, 16.0, 16.0),
        (Right, Top, -16.0, 16.0),
        (Left, Bottom, 16.0, -16.0),
        (Right, Bottom, -16.0, -16.0),
    ] {
        img = underlay(x_place, y_place, (dx, dy), [img, star.clone()]);
    }
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_offset1() {
    let img = underlay(
        Center,
        Middle,
        (10.0, 10.0),
        [circle(40.0, fill(c("red"))), circle(40.0, fill(c("blue")))],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_offset2() {
    let navy = || circle(10.0, fill(c("navy")));
    let eyes = underlay(Center, Middle, (-30.0, 0.0), [navy(), navy()]);
    let img = underlay(
        Center,
        Middle,
        (0.0, -10.0),
        [circle(40.0, fill(c("gray"))), eyes],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_variadic1() {
    let s = || fill_opacity(c("gray"), 0.4);
    let img = underlay(
        Right,
        Bottom,
        (0.0, 0.0),
        [
            square(50.0, s()),
            square(40.0, s()),
            square(30.0, s()),
            square(20.0, s()),
        ],
    );
    assert_snapshot!(svg(&img));
}

#[test]
fn underlay_xy1() {
    let r = || rectangle(20.0, 20.0, stroke(c("black")));
    assert_snapshot!(svg(&underlay_xy(&r(), 20.0, 0.0, &r())));
}

#[test]
fn underlay_xy2() {
    let (red, black) = (
        rectangle(20.0, 20.0, fill(c("red"))),
        rectangle(20.0, 20.0, fill(c("black"))),
    );
    assert_snapshot!(svg(&underlay_xy(&red, 10.0, 10.0, &black)));
}

#[test]
fn underlay_xy3() {
    let (red, black) = (
        rectangle(20.0, 20.0, fill(c("red"))),
        rectangle(20.0, 20.0, fill(c("black"))),
    );
    assert_snapshot!(svg(&underlay_xy(&red, -10.0, -10.0, &black)));
}

#[test]
fn underlay_xy4() {
    let dot = || ellipse(10.0, 10.0, fill(c("forestgreen")));
    let inner = underlay_xy(
        &ellipse(40.0, 40.0, fill(c("lightgray"))),
        10.0,
        15.0,
        &dot(),
    );
    assert_snapshot!(svg(&underlay_xy(&inner, 20.0, 15.0, &dot())));
}

fn wedge(radius: f64, angle: f64, style: Style) -> Image {
    Image::wedge(len(radius), WedgeAngle::new(angle).unwrap(), style).unwrap()
}

#[test]
fn wedge1() {
    assert_snapshot!(svg(&wedge(40.0, 90.0, fill(c("red")))));
}

#[test]
fn wedge2() {
    assert_snapshot!(svg(&wedge(40.0, 270.0, fill(c("blue")))));
}

#[test]
fn wedge3() {
    assert_snapshot!(svg(&wedge(40.0, 360.0, fill(c("blue")))));
}

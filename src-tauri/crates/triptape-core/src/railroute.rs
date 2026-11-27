//! Shinkansen paths for ground legs: a small table of lines as ordered
//! station coordinates. A leg whose ends both lie near a station is drawn
//! along the rails between them (Tokyo to Kyoto bends through Shizuoka and
//! Nagoya), not as a straight line across the mountains.

use crate::geo::haversine_m;
use crate::searoute::Node;
use std::collections::BinaryHeap;
use std::sync::OnceLock;

/// How far a stop may sit from its nearest station and still count as
/// served by it (Hakone to Odawara, central Tokyo to Tokyo station).
const SNAP_RADIUS_M: f64 = 40_000.0;

/// Each line's stations from one terminus to the other, as
/// `(station, lat, lon)`. Lines join at stations of the same name.
const LINES: &[&[(&str, f64, f64)]] = &[
    // Tokaido
    &[
        ("Tokyo", 35.6812, 139.7671),
        ("Shinagawa", 35.6285, 139.7387),
        ("Shin-Yokohama", 35.5067, 139.6176),
        ("Odawara", 35.2564, 139.1550),
        ("Atami", 35.1039, 139.0777),
        ("Mishima", 35.1268, 138.9111),
        ("Shin-Fuji", 35.1419, 138.6644),
        ("Shizuoka", 34.9718, 138.3889),
        ("Kakegawa", 34.7689, 138.0133),
        ("Hamamatsu", 34.7039, 137.7347),
        ("Toyohashi", 34.7627, 137.3820),
        ("Mikawa-Anjo", 34.9686, 137.0587),
        ("Nagoya", 35.1706, 136.8816),
        ("Gifu-Hashima", 35.3158, 136.6860),
        ("Maibara", 35.3144, 136.2898),
        ("Kyoto", 34.9858, 135.7588),
        ("Shin-Osaka", 34.7335, 135.5003),
    ],
    // Sanyo
    &[
        ("Shin-Osaka", 34.7335, 135.5003),
        ("Shin-Kobe", 34.7090, 135.1960),
        ("Nishi-Akashi", 34.6637, 134.9667),
        ("Himeji", 34.8265, 134.6905),
        ("Aioi", 34.8057, 134.4659),
        ("Okayama", 34.6667, 133.9170),
        ("Shin-Kurashiki", 34.5830, 133.7340),
        ("Fukuyama", 34.4898, 133.3622),
        ("Mihara", 34.3995, 133.0846),
        ("Higashi-Hiroshima", 34.4290, 132.7410),
        ("Hiroshima", 34.3975, 132.4753),
        ("Shin-Iwakuni", 34.1738, 132.1706),
        ("Tokuyama", 34.0495, 131.8070),
        ("Shin-Yamaguchi", 34.0940, 131.6890),
        ("Asa", 34.0090, 131.2300),
        ("Shin-Shimonoseki", 33.9990, 130.9300),
        ("Kokura", 33.8862, 130.8826),
        ("Hakata", 33.5897, 130.4207),
    ],
    // Kyushu
    &[
        ("Hakata", 33.5897, 130.4207),
        ("Shin-Tosu", 33.3744, 130.5210),
        ("Kurume", 33.3000, 130.5170),
        ("Chikugo-Funagoya", 33.1910, 130.4850),
        ("Shin-Omuta", 33.0309, 130.4724),
        ("Shin-Tamana", 32.9380, 130.5500),
        ("Kumamoto", 32.7900, 130.6890),
        ("Shin-Yatsushiro", 32.5070, 130.6200),
        ("Izumi", 32.0903, 130.3512),
        ("Sendai (Kagoshima)", 31.8330, 130.2980),
        ("Kagoshima-Chuo", 31.5840, 130.5410),
    ],
    // Tohoku
    &[
        ("Tokyo", 35.6812, 139.7671),
        ("Ueno", 35.7141, 139.7774),
        ("Omiya", 35.9062, 139.6240),
        ("Oyama", 36.3128, 139.8002),
        ("Utsunomiya", 36.5590, 139.8990),
        ("Shin-Shirakawa", 37.1180, 140.2000),
        ("Koriyama", 37.3980, 140.3880),
        ("Fukushima", 37.7540, 140.4590),
        ("Shiroishi-Zao", 37.9980, 140.6130),
        ("Sendai", 38.2606, 140.8824),
        ("Furukawa", 38.5740, 140.9560),
        ("Ichinoseki", 38.9340, 141.1400),
        ("Mizusawa-Esashi", 39.1370, 141.1700),
        ("Kitakami", 39.2870, 141.1170),
        ("Shin-Hanamaki", 39.3880, 141.1140),
        ("Morioka", 39.7011, 141.1360),
        ("Iwate-Numakunai", 40.1725, 141.2778),
        ("Ninohe", 40.2710, 141.3000),
        ("Hachinohe", 40.5121, 141.4172),
        ("Shichinohe-Towada", 40.7150, 141.1350),
        ("Shin-Aomori", 40.8277, 140.6928),
    ],
    // Hokkaido
    &[
        ("Shin-Aomori", 40.8277, 140.6928),
        ("Okutsugaru-Imabetsu", 41.1904, 140.4719),
        ("Kikonai", 41.6757, 140.4366),
        ("Shin-Hakodate-Hokuto", 41.9045, 140.6483),
    ],
    // Joetsu
    &[
        ("Omiya", 35.9062, 139.6240),
        ("Kumagaya", 36.1393, 139.3813),
        ("Honjo-Waseda", 36.2362, 139.1688),
        ("Takasaki", 36.3220, 139.0120),
        ("Jomo-Kogen", 36.7270, 138.9990),
        ("Echigo-Yuzawa", 36.9373, 138.8083),
        ("Nagaoka", 37.4480, 138.8540),
        ("Niigata", 37.9120, 139.0610),
    ],
    // Hokuriku
    &[
        ("Takasaki", 36.3220, 139.0120),
        ("Karuizawa", 36.3427, 138.6314),
        ("Ueda", 36.3957, 138.2527),
        ("Nagano", 36.6430, 138.1880),
        ("Iiyama", 36.8510, 138.3630),
        ("Joetsu-Myoko", 37.0560, 138.2470),
        ("Itoigawa", 37.0440, 137.8610),
        ("Toyama", 36.7010, 137.2130),
        ("Kanazawa", 36.5780, 136.6480),
        ("Fukui", 36.0620, 136.2240),
        ("Tsuruga", 35.6560, 136.0720),
    ],
];

type Pos = (f64, f64);

/// Every distinct station and the rail neighbours it is joined to.
struct Network {
    stations: Vec<(&'static str, Pos)>,
    links: Vec<Vec<usize>>,
}

fn network() -> Network {
    let mut stations: Vec<(&'static str, Pos)> = Vec::new();
    let mut links: Vec<Vec<usize>> = Vec::new();
    for line in LINES {
        let mut prev: Option<usize> = None;
        for &(name, lat, lon) in *line {
            let at = stations
                .iter()
                .position(|(n, _)| *n == name)
                .unwrap_or_else(|| {
                    stations.push((name, (lat, lon)));
                    links.push(Vec::new());
                    stations.len() - 1
                });
            if let Some(p) = prev {
                links[p].push(at);
                links[at].push(p);
            }
            prev = Some(at);
        }
    }
    Network { stations, links }
}

impl Network {
    /// The station nearest `p`, if one is within [`SNAP_RADIUS_M`].
    fn snap(&self, p: Pos) -> Option<usize> {
        self.stations
            .iter()
            .enumerate()
            .map(|(i, (_, q))| (i, haversine_m(p, *q)))
            .filter(|(_, d)| *d <= SNAP_RADIUS_M)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Shortest run of stations from `a` to `b` (Dijkstra over the links).
    fn walk(&self, a: usize, b: usize) -> Option<Vec<usize>> {
        let mut dist = vec![f64::INFINITY; self.stations.len()];
        let mut from = vec![usize::MAX; self.stations.len()];
        let mut heap = BinaryHeap::new();
        dist[a] = 0.0;
        heap.push(Node(0.0, a));
        while let Some(Node(d, i)) = heap.pop() {
            if i == b {
                break;
            }
            if d > dist[i] {
                continue;
            }
            for &j in &self.links[i] {
                let nd = d + haversine_m(self.stations[i].1, self.stations[j].1);
                if nd < dist[j] {
                    dist[j] = nd;
                    from[j] = i;
                    heap.push(Node(nd, j));
                }
            }
        }
        if dist[b].is_infinite() {
            return None;
        }
        let mut run = vec![b];
        while run[run.len() - 1] != a {
            run.push(from[run[run.len() - 1]]);
        }
        run.reverse();
        Some(run)
    }
}

/// The rail path from `a` to `b` (both included, `lat, lon`): the real
/// stops joined straight to their nearest stations, with the stations of
/// the line between. `None` when either end is far from every line, or
/// both snap to the same station (no train needed).
pub fn route(a: Pos, b: Pos) -> Option<Vec<Pos>> {
    static NETWORK: OnceLock<Network> = OnceLock::new();
    let net = NETWORK.get_or_init(network);
    let (sa, sb) = (net.snap(a)?, net.snap(b)?);
    if sa == sb {
        return None;
    }
    let run = net.walk(sa, sb)?;
    let pos = |i: usize| net.stations[i].1;
    // A stop that sits exactly on its station is not repeated.
    let mut path = vec![a];
    for p in run.iter().map(|&i| pos(i)).chain([b]) {
        if path.last() != Some(&p) {
            path.push(p);
        }
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO: Pos = (35.6812, 139.7671);
    const KYOTO: Pos = (34.9858, 135.7588);
    const OSAKA: Pos = (34.7025, 135.4959);

    /// A station's position, from the table.
    fn station(name: &str) -> Pos {
        let (_, pos) = network()
            .stations
            .into_iter()
            .find(|(n, _)| *n == name)
            .expect(name);
        pos
    }

    fn near(path: &[Pos], p: Pos, km: f64) -> bool {
        path.iter().any(|q| haversine_m(*q, p) < km * 1000.0)
    }

    #[test]
    fn tokyo_to_kyoto_passes_shizuoka_and_nagoya() {
        let path = route(TOKYO, KYOTO).expect("routed");
        assert!(near(&path, station("Shizuoka"), 1.0), "Shizuoka");
        assert!(near(&path, station("Nagoya"), 1.0), "Nagoya");
        assert_eq!(path[0], TOKYO);
        assert_eq!(path[path.len() - 1], KYOTO);
    }

    #[test]
    fn osaka_snaps_to_shin_osaka_and_kyoto_is_on_the_way() {
        let path = route(TOKYO, OSAKA).expect("routed");
        assert!(near(&path, KYOTO, 1.0));
        let kyoto_to_osaka = route(KYOTO, OSAKA).expect("routed");
        assert!(kyoto_to_osaka.len() <= 4, "{kyoto_to_osaka:?}");
    }

    #[test]
    fn hakone_snaps_to_odawara_with_no_detour() {
        let hakone = (35.242, 139.05);
        let shinjuku = (35.69, 139.7);
        let path = route(hakone, shinjuku).expect("routed");
        assert_eq!(path[1], station("Odawara"));
        let len: f64 = path.windows(2).map(|w| haversine_m(w[0], w[1])).sum();
        assert!(len < 1.3 * haversine_m(hakone, shinjuku), "{len}");
    }

    #[test]
    fn a_stop_boards_at_its_nearest_station_whichever_way_it_is_headed() {
        // Hakone boards at Odawara even for Kyoto, as the traveller does;
        // skipping ahead to Atami would by the same geometry skip
        // Shin-Hakodate-Hokuto for Hakodate.
        let hakone = (35.242, 139.05);
        let out = route(hakone, KYOTO).expect("routed");
        assert_eq!(out[1], station("Odawara"));
        assert_eq!(out[2], station("Atami"));
    }

    #[test]
    fn a_stop_on_its_station_is_not_repeated() {
        let path = route(TOKYO, KYOTO).expect("routed");
        assert!(path.windows(2).all(|w| w[0] != w[1]), "{path:?}");
        assert_eq!(path[1], station("Shinagawa"));
        assert_eq!(path[path.len() - 2], station("Maibara"));
    }

    #[test]
    fn hakodate_to_tokyo_goes_via_sendai() {
        let hakodate = (41.7687, 140.7288);
        let path = route(hakodate, TOKYO).expect("routed");
        assert!(near(&path, station("Sendai"), 1.0), "Sendai");
        assert!(near(&path, station("Shin-Aomori"), 1.0), "Shin-Aomori");
        assert!(
            near(&path, station("Shin-Hakodate-Hokuto"), 1.0),
            "Shin-Hakodate-Hokuto"
        );
    }

    #[test]
    fn tokyo_to_hiroshima_joins_tokaido_and_sanyo() {
        let path = route(TOKYO, (34.3853, 132.4553)).expect("routed");
        assert!(near(&path, station("Shin-Osaka"), 1.0), "Shin-Osaka");
    }

    #[test]
    fn far_from_any_line_or_one_station_is_not_routed() {
        assert!(route((26.2124, 127.6809), TOKYO).is_none());
        assert!(route(TOKYO, (30.0, 160.0)).is_none());
        assert!(route(TOKYO, (35.675, 139.77)).is_none());
    }

    #[test]
    fn every_station_is_reachable_from_tokyo() {
        let net = network();
        let tokyo = net.snap(TOKYO).unwrap();
        for (i, (name, _)) in net.stations.iter().enumerate() {
            assert!(net.walk(tokyo, i).is_some(), "{name}");
        }
    }
}

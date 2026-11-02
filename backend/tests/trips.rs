use ryu_places::{
    models::{MarkStatus, NewPlace},
    tenancy::Stores,
    trips::*,
};

fn stop(name: &str, lat: f64, lng: f64, day: Option<i64>) -> NewStop {
    NewStop {
        name: Some(name.to_owned()),
        lat: Some(lat),
        lng: Some(lng),
        category: Some("sight".to_owned()),
        day,
        ..Default::default()
    }
}

fn names(day: &TripDay) -> Vec<&str> {
    day.stops.iter().map(|stop| stop.name.as_str()).collect()
}

#[tokio::test]
async fn orgs_never_see_each_others_maps() {
    let stores = Stores::memory();
    let acme = stores.get("org:acme").await.unwrap();
    let other = stores.get("org:other").await.unwrap();
    acme.add_place(
        "ana",
        NewPlace {
            name: "Ana's cafe".into(),
            address: None,
            lat: 1.0,
            lng: 1.0,
            category: "cafe".into(),
            url: None,
            note: None,
            list_id: None,
            external_id: None,
            status: Some(MarkStatus::Want),
            visited_on: None,
            rating: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(acme.counts().await.unwrap(), 1);
    assert_eq!(other.counts().await.unwrap(), 0);
    // The same tenant always resolves to the same store.
    assert_eq!(
        stores
            .get("org:acme")
            .await
            .unwrap()
            .counts()
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn itinerary_days_follow_dates_and_moves_renumber() {
    let store = Stores::memory().get("local").await.unwrap();
    let trip = store
        .create_trip(
            "local",
            NewTrip {
                name: "Kyoto".into(),
                start_date: Some("2027-04-01".into()),
                end_date: Some("2027-04-03".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(trip.trip.day_count, 3);
    assert_eq!(trip.days[2].date.as_deref(), Some("2027-04-03"));
    let id = trip.trip.id.clone();

    store
        .add_stop(
            "local",
            &id,
            stop("Fushimi Inari", 34.967, 135.773, Some(1)),
        )
        .await
        .unwrap();
    store
        .add_stop("local", &id, stop("Kiyomizu", 34.995, 135.785, Some(1)))
        .await
        .unwrap();
    let detail = store
        .add_stop("local", &id, stop("Arashiyama", 35.017, 135.677, None))
        .await
        .unwrap();
    assert_eq!(names(&detail.days[0]), ["Fushimi Inari", "Kiyomizu"]);
    assert!(detail.days[0].distance_km > 3.0);
    assert_eq!(detail.ideas.len(), 1);

    // Move the idea to the front of day 1.
    let idea = detail.ideas[0].id.clone();
    let moved = store
        .update_stop(
            "local",
            &id,
            &idea,
            StopPatch {
                day: Some(Some(1)),
                position: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        names(&moved.days[0]),
        ["Arashiyama", "Fushimi Inari", "Kiyomizu"]
    );
    assert!(moved.ideas.is_empty());
    let positions: Vec<i64> = moved.days[0].stops.iter().map(|s| s.position).collect();
    assert_eq!(positions, [0, 1, 2]);

    // Shortening the trip parks later days in ideas instead of deleting them.
    let kiyomizu = moved.days[0].stops[2].id.clone();
    store
        .update_stop(
            "local",
            &id,
            &kiyomizu,
            StopPatch {
                day: Some(Some(3)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let short = store
        .update_trip(
            "local",
            &id,
            TripPatch {
                end_date: Some(Some("2027-04-02".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(short.trip.day_count, 2);
    assert_eq!(short.ideas.len(), 1);
    assert_eq!(short.ideas[0].name, "Kiyomizu");

    let bad = store
        .add_stop("local", &id, stop("Nowhere", 0.0, 0.0, Some(9)))
        .await
        .unwrap_err();
    assert_eq!(
        bad.downcast_ref::<TripError>(),
        Some(&TripError::DayOutOfRange)
    );
    let half = store
        .create_trip(
            "local",
            NewTrip {
                name: "x".into(),
                start_date: Some("2027-01-01".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        half.downcast_ref::<TripError>(),
        Some(&TripError::HalfDates)
    );
}

#[tokio::test]
async fn optimize_day_shortens_a_zigzag_and_keeps_the_start() {
    let store = Stores::memory().get("local").await.unwrap();
    let trip = store
        .create_trip(
            "local",
            NewTrip {
                name: "Line".into(),
                day_count: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let id = trip.trip.id;
    for (name, lng) in [
        ("Hotel", 0.0),
        ("C", 0.03),
        ("A", 0.01),
        ("D", 0.04),
        ("B", 0.02),
    ] {
        store
            .add_stop("local", &id, stop(name, 0.0, lng, Some(1)))
            .await
            .unwrap();
    }
    let before = store.trip(&id).await.unwrap().days[0].distance_km;
    let after = store.optimize_day("local", &id, 1).await.unwrap();
    assert_eq!(names(&after.days[0]), ["Hotel", "A", "B", "C", "D"]);
    assert!(after.days[0].distance_km < before);
    assert_eq!(optimize_order(&[(0.0, 0.0), (0.0, 1.0)]), [0, 1]);
}

#[tokio::test]
async fn expenses_split_exactly_and_settle_up() {
    let store = Stores::memory().get("org:fam").await.unwrap();
    for member in ["ana", "ben", "cy"] {
        store.ensure_member(member).await.unwrap();
    }
    let trip = store
        .create_trip(
            "ana",
            NewTrip {
                name: "Lisbon".into(),
                budget_minor: Some(100_000),
                currency: Some("eur".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(trip.trip.currency, "EUR");
    let id = trip.trip.id;
    // Ana pays 100.00 split three ways: 33.34 / 33.33 / 33.33.
    store
        .add_expense(
            "ana",
            &id,
            NewExpense {
                title: "Dinner".into(),
                amount_minor: 10_000,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Ben pays 30.00 for himself and Cy.
    let detail = store
        .add_expense(
            "ben",
            &id,
            NewExpense {
                title: "Tram".into(),
                amount_minor: 3_000,
                split_between: vec!["ben".into(), "cy".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let net = |member: &str| {
        detail
            .budget
            .balances
            .iter()
            .find(|b| b.member_id == member)
            .map_or(0, |b| b.net_minor)
    };
    assert_eq!(net("ana"), 10_000 - 3_334);
    assert_eq!(net("ben"), 3_000 - 3_333 - 1_500);
    assert_eq!(net("cy"), -3_333 - 1_500);
    assert_eq!(net("ana") + net("ben") + net("cy"), 0);
    let paid: i64 = detail
        .budget
        .settlements
        .iter()
        .filter(|s| s.to == "ana")
        .map(|s| s.amount_minor)
        .sum();
    assert_eq!(paid, net("ana"));
    assert_eq!(detail.budget.totals[0].spent_minor, 13_000);
    assert_eq!(detail.budget.totals[0].budget_minor, Some(100_000));

    let stranger = store
        .add_expense(
            "ana",
            &id,
            NewExpense {
                title: "x".into(),
                amount_minor: 1,
                split_between: vec!["zed".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        stranger.downcast_ref::<TripError>(),
        Some(&TripError::UnknownMember)
    );
}

#[tokio::test]
async fn bookings_and_checklist_round_trip() {
    let store = Stores::memory().get("local").await.unwrap();
    let trip = store
        .create_trip(
            "local",
            NewTrip {
                name: "Rome".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let id = trip.trip.id;
    let detail = store
        .add_reservation(
            "local",
            &id,
            NewReservation {
                kind: Some("Flight".into()),
                title: "FCO arrival".into(),
                starts_at: Some("2027-05-01T09:30".into()),
                confirmation: Some("ABC123".into()),
                cost_minor: Some(25_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(detail.reservations[0].kind, "flight");
    assert_eq!(detail.budget.totals[0].booked_minor, 25_000);
    let bad = store
        .add_reservation(
            "local",
            &id,
            NewReservation {
                title: "x".into(),
                starts_at: Some("tomorrow".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        bad.downcast_ref::<TripError>(),
        Some(&TripError::InvalidDateTime)
    );

    let listed = store
        .add_checklist_item(
            "local",
            &id,
            NewChecklistItem {
                text: "Passports".into(),
                assignee: None,
            },
        )
        .await
        .unwrap();
    let item = listed.checklist[0].id.clone();
    let done = store
        .update_checklist_item(
            "local",
            &id,
            &item,
            ChecklistPatch {
                done: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(done.checklist[0].done);
    let removed = store
        .remove_trip_item("local", &id, TripItemKind::Checklist, &item)
        .await
        .unwrap();
    assert!(removed.checklist.is_empty());

    store.delete_trip("local", &id).await.unwrap();
    assert!(store.trips().await.unwrap().is_empty());
}

#[tokio::test]
async fn amounts_above_the_cap_are_rejected() {
    let store = Stores::memory().get("local").await.unwrap();
    let invalid = |error: anyhow::Error| {
        assert_eq!(
            error.downcast_ref::<TripError>(),
            Some(&TripError::InvalidAmount)
        );
    };
    invalid(
        store
            .create_trip(
                "local",
                NewTrip {
                    name: "Moon".into(),
                    budget_minor: Some(MAX_AMOUNT_MINOR + 1),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
    );
    let trip = store
        .create_trip(
            "local",
            NewTrip {
                name: "Moon".into(),
                budget_minor: Some(MAX_AMOUNT_MINOR),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let id = trip.trip.id;
    invalid(
        store
            .update_trip(
                "local",
                &id,
                TripPatch {
                    budget_minor: Some(Some(i64::MAX)),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
    );
    invalid(
        store
            .add_reservation(
                "local",
                &id,
                NewReservation {
                    title: "Rocket".into(),
                    cost_minor: Some(MAX_AMOUNT_MINOR + 1),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
    );
    // The failure scenario: a model mis-scales minor units twice.
    for _ in 0..2 {
        invalid(
            store
                .add_expense(
                    "local",
                    &id,
                    NewExpense {
                        title: "Fuel".into(),
                        amount_minor: 9_223_372_036_854_775_000,
                        ..Default::default()
                    },
                )
                .await
                .unwrap_err(),
        );
    }
    let detail = store
        .add_expense(
            "local",
            &id,
            NewExpense {
                title: "Fuel".into(),
                amount_minor: MAX_AMOUNT_MINOR,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(detail.budget.totals[0].spent_minor, MAX_AMOUNT_MINOR);
    assert!(store.trip(&id).await.is_ok());
}

#[test]
fn balances_saturate_instead_of_overflowing() {
    // Rows written before the cap existed must not panic or wrap on read.
    let huge = |id: &str| Expense {
        id: id.into(),
        trip_id: "t".into(),
        title: "x".into(),
        amount_minor: i64::MAX - 100,
        currency: "USD".into(),
        paid_by: "ana".into(),
        split_between: vec!["ben".into()],
        spent_on: None,
        category: "other".into(),
        created_by: "ana".into(),
        created_at: 0,
    };
    let net = balances(&[huge("a"), huge("b")]);
    assert_eq!(net[&("USD".to_owned(), "ana".to_owned())], i64::MAX);
    assert_eq!(net[&("USD".to_owned(), "ben".to_owned())], i64::MIN);
    let transfers = settle(&net);
    assert_eq!(
        transfers,
        vec![(
            "USD".to_owned(),
            "ben".to_owned(),
            "ana".to_owned(),
            i64::MAX
        )]
    );
}

#[tokio::test]
async fn rejected_stop_move_leaves_the_stop_untouched() {
    let store = Stores::memory().get("local").await.unwrap();
    let trip = store
        .create_trip(
            "local",
            NewTrip {
                name: "Oslo".into(),
                start_date: Some("2027-06-01".into()),
                end_date: Some("2027-06-02".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let id = trip.trip.id;
    let detail = store
        .add_stop("local", &id, stop("Opera", 59.907, 10.753, Some(1)))
        .await
        .unwrap();
    let opera = detail.days[0].stops[0].id.clone();
    let error = store
        .update_stop(
            "local",
            &id,
            &opera,
            StopPatch {
                day: Some(Some(99)),
                name: Some("Renamed".into()),
                note: Some(Some("half-applied".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<TripError>(),
        Some(&TripError::DayOutOfRange)
    );
    let after = store.trip(&id).await.unwrap();
    let unchanged = &after.days[0].stops[0];
    assert_eq!(unchanged.name, "Opera");
    assert_eq!(unchanged.note, None);
}

#[tokio::test]
async fn repeated_split_members_are_charged_once() {
    let store = Stores::memory().get("org:fam").await.unwrap();
    for member in ["ana", "ben", "cy"] {
        store.ensure_member(member).await.unwrap();
    }
    let trip = store
        .create_trip(
            "ana",
            NewTrip {
                name: "Porto".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let detail = store
        .add_expense(
            "cy",
            &trip.trip.id,
            NewExpense {
                title: "Taxi".into(),
                amount_minor: 2_000,
                split_between: vec!["ana".into(), "ben".into(), " ana ".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(detail.expenses[0].split_between, ["ana", "ben"]);
    let net = |member: &str| {
        detail
            .budget
            .balances
            .iter()
            .find(|b| b.member_id == member)
            .map_or(0, |b| b.net_minor)
    };
    assert_eq!(net("ana"), -1_000);
    assert_eq!(net("ben"), -1_000);
    assert_eq!(net("cy"), 2_000);
}

use ryu_places::{models::*, store::PlaceStore, validation::ValidationError};

fn ramen() -> NewPlace {
    NewPlace {
        name: " Ichiran Shibuya ".to_owned(),
        address: Some("Tokyo".to_owned()),
        lat: 35.658,
        lng: 139.7016,
        category: "Food".to_owned(),
        url: None,
        note: None,
        list_id: None,
        external_id: Some("osm:n123".to_owned()),
        status: Some(MarkStatus::Want),
        visited_on: None,
        rating: None,
    }
}

fn been(rating: i64) -> MarkInput {
    MarkInput {
        status: MarkStatus::Been,
        visited_on: Some("2026-09-01".to_owned()),
        rating: Some(rating),
        note: Some("Get the extra-firm noodles".to_owned()),
    }
}

#[tokio::test]
async fn members_keep_their_own_marks_on_one_shared_place() {
    let store = PlaceStore::open_memory().unwrap();
    let saved = store.add_place("mum", ramen()).await.unwrap();
    assert_eq!(saved.name, "Ichiran Shibuya");
    assert_eq!(saved.category, "food");
    assert_eq!(saved.added_by, "mum");
    assert_eq!(saved.marks.len(), 1);

    // A second member saving the same geocoder hit converges on the same pin.
    let again = store.add_place("kid", ramen()).await.unwrap();
    assert_eq!(again.id, saved.id);
    assert_eq!(again.marks.len(), 2);

    let after = store.set_mark("mum", &saved.id, been(5)).await.unwrap();
    let mum = after
        .marks
        .iter()
        .find(|mark| mark.member_id == "mum")
        .unwrap();
    let kid = after
        .marks
        .iter()
        .find(|mark| mark.member_id == "kid")
        .unwrap();
    assert_eq!(mum.status, MarkStatus::Been);
    assert_eq!(mum.rating, Some(5));
    assert_eq!(kid.status, MarkStatus::Want);
    assert_eq!(kid.member_name, "Member 2");

    let mum_been = store
        .list_places(
            "kid",
            PlaceFilters {
                status: Some("been".into()),
                member: Some("mum".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(mum_been.len(), 1);
    let my_been = store
        .list_places(
            "kid",
            PlaceFilters {
                status: Some("been".into()),
                member: Some("me".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(my_been.is_empty());

    let summary = store.summary().await.unwrap();
    assert_eq!(summary.places, 1);
    assert_eq!(summary.want, 1);
    assert_eq!(summary.been, 1);
    assert_eq!(summary.members.len(), 2);

    let cleared = store.clear_mark("kid", &saved.id).await.unwrap();
    assert_eq!(cleared.marks.len(), 1);
    assert!(store.clear_mark("kid", &saved.id).await.is_err());

    let feed = store.activity(None).await.unwrap();
    let kinds = feed.iter().map(|row| row.kind.as_str()).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        ["unmarked", "been", "want", "joined", "want", "saved", "joined"]
    );
}

#[tokio::test]
async fn lists_file_places_and_deleting_a_list_keeps_its_places() {
    let store = PlaceStore::open_memory().unwrap();
    let trip = store
        .create_list(
            "local",
            NewList {
                name: "Tokyo 2027".into(),
                emoji: Some("🗼".into()),
            },
        )
        .await
        .unwrap();
    let mut input = ramen();
    input.list_id = Some(trip.id.clone());
    let place = store.add_place("local", input).await.unwrap();
    assert_eq!(store.lists().await.unwrap()[0].place_count, 1);

    let filed = store
        .list_places(
            "local",
            PlaceFilters {
                list: Some(trip.id.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filed.len(), 1);

    store.delete_list("local", &trip.id).await.unwrap();
    let kept = store.get_place(&place.id).await.unwrap();
    assert_eq!(kept.list_id, None);

    let missing = store
        .update_place(
            "local",
            &place.id,
            PlacePatch {
                list_id: Some(Some(trip.id)),
                ..Default::default()
            },
        )
        .await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn validation_rejects_bad_places_and_marks() {
    let store = PlaceStore::open_memory().unwrap();
    let mut far = ramen();
    far.lat = 91.0;
    let error = store.add_place("local", far).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<ValidationError>(),
        Some(&ValidationError::InvalidCoordinates)
    );

    let mut odd = ramen();
    odd.category = "spaceport".into();
    assert!(store.add_place("local", odd).await.is_err());

    let mut early = ramen();
    early.visited_on = Some("2026-01-01".into());
    let error = store.add_place("local", early).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<ValidationError>(),
        Some(&ValidationError::WantHasVisit)
    );

    let place = store.add_place("local", ramen()).await.unwrap();
    let error = store
        .set_mark("local", &place.id, been(6))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ValidationError>(),
        Some(&ValidationError::InvalidRating)
    );

    let bad_url = PlacePatch {
        url: Some(Some("javascript:alert(1)".into())),
        ..Default::default()
    };
    assert!(store
        .update_place("local", &place.id, bad_url)
        .await
        .is_err());
}

#[tokio::test]
async fn members_rename_themselves_and_search_text_is_literal() {
    let store = PlaceStore::open_memory().unwrap();
    let owner = store.ensure_member("local").await.unwrap();
    assert_eq!(owner.name, "Owner");
    let renamed = store
        .update_member(
            "local",
            MemberPatch {
                name: Some("Dad".into()),
                color: Some("#ABCDEF".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "Dad");
    assert_eq!(renamed.color, "#abcdef");
    assert!(store
        .update_member(
            "local",
            MemberPatch {
                name: None,
                color: Some("red".into())
            }
        )
        .await
        .is_err());

    store.add_place("local", ramen()).await.unwrap();
    let hits = store
        .list_places(
            "local",
            PlaceFilters {
                q: Some("ichiran".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let wildcard = store
        .list_places(
            "local",
            PlaceFilters {
                q: Some("%".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(wildcard.is_empty());
}

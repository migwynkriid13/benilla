//! A `PlayerModel` owns its unit copy (`0x5059a0`), independently of its global frame name.
//! Hide destroys the copy (`0x505ce0`); show and `RefreshUnit` rebuild it (`0x505d00`).

use std::collections::HashMap;

use benilla_assets::{M2Model, PortraitCamera};
use benilla_ui::script::{UiScript, UnitModelPaneFrame};
use benilla_ui::widget::{
    FrameHandle, ModelFileFacts, ModelFog, ModelLight, ModelPlayHead, SequenceFacts,
};
use benilla_world::rig_anim::SceneClock;
use benilla_world::rig_palette::{RigPaletteMirrors, RigPalettes};
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{OrthographicProjection, Projection, RenderTarget, ScalingMode};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::render::renderer::{RenderDevice, RenderQueue};

use super::{
    booth_view_shape, framing, model_pane_booth, new_target_image, sync_body_booth, Booth,
    BoothCam, BoothLight, BoothPanes, Booths, DressedLook, PortraitImages, PortraitSource, Turn,
    PAPERDOLL_SIZE, PORTRAIT_FOV, UI_MODEL_CAM_LAYERS, UI_MODEL_CAM_LAYER_BASE,
};

// Deviation: a pane needs an offscreen view here, so only eight generic unit panes draw at once,
// as the file panes' perspective pool is bounded. Hidden panes release their slot immediately.
const PANE_LIMIT: usize = 8;
const LAYER_BASE: usize = UI_MODEL_CAM_LAYER_BASE + UI_MODEL_CAM_LAYERS;
const MIRROR_KEYS: [&str; PANE_LIMIT] = [
    "unit_model_0",
    "unit_model_1",
    "unit_model_2",
    "unit_model_3",
    "unit_model_4",
    "unit_model_5",
    "unit_model_6",
    "unit_model_7",
];

/// The image key includes the binding revision, so a rebind never samples the previous subject.
pub(crate) fn unit_pane_slot(handle: FrameHandle, instance: u64) -> String {
    format!("unit-model:{handle:?}:{instance}")
}

struct UnitPane {
    pool: usize,
    instance: u64,
    slot: String,
    camera: Entity,
    root: Entity,
    light: BoothLight,
    scene: Option<(ModelLight, Option<ModelFog>)>,
    last_pose: Option<(f32, f32)>,
    display: Option<u32>,
    frozen: Option<(PortraitCamera, Transform)>,
    play: Option<ModelPlayHead>,
    clock_ms: u64,
}

/// No views exist until a visible, unit-bound `PlayerModel` asks for its own copy.
#[derive(Resource, Default)]
pub(super) struct UnitPanes {
    session: u64,
    entries: HashMap<FrameHandle, UnitPane>,
}

impl UnitPanes {
    fn free_slot(&self) -> Option<usize> {
        (0..PANE_LIMIT).find(|slot| self.entries.values().all(|pane| pane.pool != *slot))
    }

    /// Session replacement, hides and rebindings all retire the old copy before its next paint.
    fn retire(&mut self, session: u64, live: &HashMap<FrameHandle, u64>) -> Vec<UnitPane> {
        let changed = self.session != session;
        self.session = session;
        let retired: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(handle, pane)| {
                (changed || live.get(handle) != Some(&pane.instance)).then_some(*handle)
            })
            .collect();
        retired
            .into_iter()
            .filter_map(|handle| self.entries.remove(&handle))
            .collect()
    }
}

#[derive(SystemParam)]
pub(super) struct UnitPaneAssets<'w> {
    images: ResMut<'w, Assets<Image>>,
    models: Option<Res<'w, Assets<M2Model>>>,
    materials: ResMut<'w, Assets<benilla_assets::materials::WowModelMaterial>>,
    creatures: Option<Res<'w, crate::entities::Creatures>>,
    device: Option<Res<'w, RenderDevice>>,
    queue: Option<Res<'w, RenderQueue>>,
    mirrors: ResMut<'w, RigPaletteMirrors>,
}

fn forget(
    pane: UnitPane,
    commands: &mut Commands,
    booths: &mut Booths,
    portraits: &mut PortraitImages,
    panes: &mut BoothPanes,
    images: &mut Assets<Image>,
    mirrors: &mut RigPaletteMirrors,
) {
    commands.entity(pane.camera).despawn();
    commands.entity(pane.root).despawn();
    if let Some(booth) = booths.0.remove(&pane.slot) {
        images.remove(booth.target.id());
    }
    portraits.0.remove(&pane.slot);
    panes.0.remove(&pane.slot);
    mirrors.0.remove(MIRROR_KEYS[pane.pool]);
}

fn spawn(
    handle: FrameHandle,
    instance: u64,
    pool: usize,
    commands: &mut Commands,
    booths: &mut Booths,
    images: &mut Assets<Image>,
) -> UnitPane {
    let slot = unit_pane_slot(handle, instance);
    let target = images.add(new_target_image(PAPERDOLL_SIZE));
    let layer = RenderLayers::layer(LAYER_BASE + pool);
    let root = commands
        .spawn((Transform::IDENTITY, Visibility::Visible, layer.clone()))
        .id();
    let camera = commands
        .spawn((
            booth_view_shape(),
            Camera {
                order: -50 + pool as isize,
                clear_color: ClearColorConfig::Custom(Color::NONE),
                is_active: false,
                ..default()
            },
            RenderTarget::Image(target.clone().into()),
            benilla_world::ffx_glow::FfxGlow::UI_PANE,
            Projection::from(bevy::camera::PerspectiveProjection {
                fov: PORTRAIT_FOV,
                near: 0.02,
                far: 100.0,
                ..default()
            }),
            layer.clone(),
            BoothCam(slot.clone()),
        ))
        .id();
    booths.0.insert(
        slot.clone(),
        Booth {
            layer,
            root,
            target,
            baked: None,
            baked_guid: None,
            snap: None,
            shown: false,
            show_rev: 0,
            wake: 0,
            live: false,
            pending: Vec::new(),
            pending_since: None,
            pipes_settling: false,
            pipes_since: None,
            aspect: 1.0,
            rigged: false,
            parked: false,
            turn: Turn::default(),
        },
    );
    UnitPane {
        pool,
        instance,
        slot,
        camera,
        root,
        light: BoothLight::default(),
        scene: None,
        last_pose: None,
        display: None,
        frozen: None,
        play: None,
        clock_ms: 0,
    }
}

/// After Lua input, each visible pane mirrors its own fixed GUID, never the token's new target.
pub(super) fn sync_unit_panes(
    mut commands: Commands,
    mut script: Option<NonSendMut<UiScript>>,
    mut registry: ResMut<UnitPanes>,
    mut booths: ResMut<Booths>,
    mut portraits: ResMut<PortraitImages>,
    mut framing_in: BoothFramingMut,
    mut assets: UnitPaneAssets,
    index: Option<Res<crate::net::GuidIndex>>,
    entities: Query<&crate::net::NetEntity>,
    look: DressedLook,
    mut palettes: ResMut<RigPalettes>,
    mut cameras: Query<(&BoothCam, &mut Transform, &mut Projection)>,
    anim_data: Option<Res<crate::creature_anim::AnimData>>,
) {
    let session = script.as_ref().map_or(0, |script| script.session());
    let visible: Vec<_> = script.as_ref().map_or_else(Vec::new, |script| {
        script
            .visible_unit_model_panes()
            .into_iter()
            .filter(|pane| pane.name.as_deref().and_then(model_pane_booth).is_none())
            .collect()
    });
    let live: HashMap<_, _> = visible
        .iter()
        .filter(|pane| {
            pane.state.unit_model_present
                && index
                    .as_ref()
                    .is_some_and(|index| index.0.contains_key(&pane.guid))
        })
        .map(|pane| (pane.handle, pane.instance))
        .collect();
    for pane in registry.retire(session, &live) {
        forget(
            pane,
            &mut commands,
            &mut booths,
            &mut portraits,
            &mut framing_in.panes,
            &mut assets.images,
            &mut assets.mirrors,
        );
    }
    let Some(script) = script.as_mut() else {
        return;
    };
    for mut pane in visible {
        let slot = unit_pane_slot(pane.handle, pane.instance);
        portraits.0.remove(&slot);
        if let Some(booth) = booths.0.get_mut(&slot) {
            booth.live = false;
        }
        let Some(unit) = index
            .as_ref()
            .and_then(|index| index.0.get(&pane.guid))
            .copied()
        else {
            script.set_unit_model_present(pane.handle, pane.instance, false);
            continue;
        };
        let Some(display) = entities.get(unit).ok().and_then(|entity| entity.display_id) else {
            script.set_unit_model_present(pane.handle, pane.instance, false);
            continue;
        };
        let model_handle = assets
            .creatures
            .as_deref()
            .and_then(|creatures| creatures.display_model(display));
        script.set_unit_model_present(pane.handle, pane.instance, model_handle.is_some());
        let model = model_handle.and_then(|handle| {
            assets
                .models
                .as_deref()
                .and_then(|models| models.get(handle))
        });
        let Some(model) = model else { continue };
        let changed = registry
            .entries
            .get(&pane.handle)
            .is_some_and(|entry| entry.display != Some(display));
        if changed {
            // A transformed unit supplies a new copy: its ready hook chooses the default camera
            // and Stand again, rather than inheriting a camera or arm from the previous model.
            script.set_unit_model_present(pane.handle, pane.instance, false);
        }
        if pane.state.unit_facts.is_none() || changed {
            script.set_unit_model_facts(pane.handle, pane.instance, facts_of(model));
        }
        let Some(prepared) = script.prepare_unit_model_paint(pane.handle, pane.instance) else {
            continue;
        };
        pane.state = prepared;
        let requested_camera = pane
            .state
            .unit_camera
            .and_then(|requested| {
                usize::try_from(requested)
                    .ok()
                    .and_then(|index| model.cameras.get(index))
            })
            .map(|camera| camera.still);
        // Size-less panes need their model facts before ApplyRect can produce drawable geometry.
        let aspect = pane.rect.width() / pane.rect.height();
        if pane.rect.width() <= 0.0
            || pane.rect.height() <= 0.0
            || !aspect.is_finite()
            || ![
                pane.state.facing,
                pane.state.scale,
                pane.state.position.0,
                pane.state.position.1,
                pane.state.position.2,
            ]
            .iter()
            .all(|value| value.is_finite())
            || pane.state.scale == 0.0
        {
            continue;
        }
        if !registry.entries.contains_key(&pane.handle) {
            let Some(pool) = registry.free_slot() else {
                continue;
            };
            let entry = spawn(
                pane.handle,
                pane.instance,
                pool,
                &mut commands,
                &mut booths,
                &mut assets.images,
            );
            registry.entries.insert(pane.handle, entry);
        }
        let entry = registry
            .entries
            .get_mut(&pane.handle)
            .expect("pane just allocated");
        if entry.display != Some(display) {
            entry.display = Some(display);
            entry.frozen = None;
            if let Some(booth) = booths.0.get_mut(&entry.slot) {
                booth.snap = None;
                commands.entity(booth.root).despawn_related::<Children>();
                super::clear_booth_rig(&mut commands, booth.root);
                booth.rigged = false;
                booth.live = false;
            }
            portraits.0.remove(&entry.slot);
        }
        let scene = (pane.state.light, pane.state.scene_fog());
        if entry.scene != Some(scene)
            || (entry.light.pane.buffer.is_none() && assets.device.is_some())
        {
            if let (Some(device), Some(queue)) = (assets.device.as_deref(), assets.queue.as_deref())
            {
                if entry.light.set_model_scene(scene.0, scene.1, device, queue) {
                    if let Some(booth) = booths.0.get_mut(&entry.slot) {
                        booth.snap = None;
                    }
                }
                if let Some(buffer) = &entry.light.pane.buffer {
                    assets
                        .mirrors
                        .0
                        .insert(MIRROR_KEYS[entry.pool], buffer.clone());
                }
            }
            entry.scene = Some(scene);
        }
        entry.light.reap_model_variants(&assets.materials);
        framing_in.panes.0.insert(entry.slot.clone(), aspect);
        sync_body_booth(
            &mut palettes,
            &entry.slot,
            Some(unit),
            pane.state.facing,
            &mut entry.last_pose,
            framing_in.gx.0,
            &mut commands,
            &mut booths,
            &mut portraits,
            &mut entry.light,
            assets.creatures.as_deref(),
            &entities,
            &look,
            &mut assets.materials,
            &mut cameras,
            anim_data.as_deref(),
            &framing_in.panes,
        );
        let Some(booth) = booths.0.get_mut(&entry.slot) else {
            continue;
        };
        if booth
            .snap
            .as_ref()
            .is_none_or(|snapshot| snapshot.unit != unit)
        {
            portraits.0.remove(&entry.slot);
            continue;
        }
        // This pane's engine clock and native per-paint turn reset own its primary track. The
        // stock windows retain their own booth turn driver; do not arm a second track here.
        booth.turn = Turn::default();
        entry.play = pane
            .state
            .unit_facts
            .as_deref()
            .and_then(|facts| pane.state.play_head(facts));
        entry.clock_ms = pane.state.clock_ms;
        let Some(anchors) = assets
            .creatures
            .as_deref()
            .and_then(|creatures| creatures.display_anchors(display))
        else {
            portraits.0.remove(&entry.slot);
            continue;
        };
        let view = unit_view(
            &pane,
            &anchors,
            requested_camera,
            framing_in.gx.0,
            &mut entry.frozen,
        );
        commands.entity(entry.root).insert(view.root);
        commands
            .entity(entry.camera)
            .insert((view.camera, view.projection));
        // A live native model is composited at the widget's own ARTWORK callback; the image
        // identity is stable while its private camera paints a new Stand pose each frame.
        booth.live = true;
        portraits.0.insert(
            entry.slot.clone(),
            PortraitSource::Live(booth.target.clone()),
        );
    }
}

/// Sample the engine-owned play head after the copy's deferred rig spawn reaches the world.
pub(super) fn sample_unit_panes(
    mut commands: Commands,
    registry: Res<UnitPanes>,
    booths: Res<Booths>,
    mut roots: Query<(
        Option<&mut AnimationPlayer>,
        Option<&benilla_assets::ModelAnimations>,
        Option<&mut SceneClock>,
    )>,
) {
    for entry in registry.entries.values() {
        if booths.0.get(&entry.slot).is_none_or(|booth| !booth.live) {
            continue;
        }
        let Ok((player, animations, clock)) = roots.get_mut(entry.root) else {
            continue;
        };
        let seconds = entry.clock_ms as f64 / 1000.0;
        if let Some(mut clock) = clock {
            clock.0 = seconds;
        } else {
            commands.entity(entry.root).insert(SceneClock(seconds));
        }
        let (Some(mut player), Some(animations)) = (player, animations) else {
            continue;
        };
        sample_pose(&mut player, animations, entry.play);
    }
}

fn sample_pose(
    player: &mut AnimationPlayer,
    animations: &benilla_assets::ModelAnimations,
    play: Option<ModelPlayHead>,
) {
    let clip = play.and_then(|play| {
        animations
            .find(play.anim_id)
            .map(|clip| (clip, play.cursor_ms))
    });
    for other in &animations.clips {
        if clip.is_none_or(|(clip, _)| clip.node != other.node) {
            player.stop(other.node);
        }
    }
    if let Some((clip, cursor_ms)) = clip {
        let active = player.play(clip.node);
        active.pause();
        active.set_weight(1.0);
        active.seek_to(cursor_ms as f32 / 1000.0);
    }
}

#[derive(SystemParam)]
pub(super) struct BoothFramingMut<'w> {
    panes: ResMut<'w, BoothPanes>,
    gx: Res<'w, super::GxAspect>,
}

struct UnitView {
    root: Transform,
    camera: Transform,
    projection: Projection,
}

fn unit_view(
    pane: &UnitModelPaneFrame,
    anchors: &super::PortraitAnchors,
    requested_camera: Option<PortraitCamera>,
    display_aspect: f32,
    frozen: &mut Option<(PortraitCamera, Transform)>,
) -> UnitView {
    let aspect = pane.rect.width() / pane.rect.height();
    let state = &pane.state;
    let root = Transform {
        translation: benilla_assets::coords::wow_to_bevy([
            state.position.0,
            state.position.1,
            state.position.2,
        ]) * pane.scale,
        rotation: Quat::from_rotation_y(state.facing),
        scale: Vec3::splat(framing::pane_model_scale(display_aspect) * state.scale * pane.scale),
    };
    if state.unit_camera.is_some() {
        if let Some(record) = requested_camera {
            // Explicit SetCamera installs a live file camera (`0x76cec0`), unlike the frozen
            // camera the PlayerModel model-ready hook chooses (`0x505890`).
            let matrix = root.to_matrix();
            let eye = matrix.transform_point3(record.eye);
            let target = matrix.transform_point3(record.target);
            let (sin, cos) = record.roll.sin_cos();
            let up = benilla_assets::coords::wow_to_bevy([0.0, -sin, cos]);
            return UnitView {
                root,
                camera: Transform::from_translation(eye).looking_at(target, up),
                projection: Projection::custom(framing::pane_projection(&record, aspect)),
            };
        }
        return orthographic_view(pane, display_aspect);
    }
    let (record, capture) = frozen.get_or_insert_with(|| {
        let capture = if anchors.pane_camera.is_some() {
            // The ready hook snapshots the camera at facing zero (`0x505935`).
            Transform {
                rotation: Quat::IDENTITY,
                ..root
            }
        } else {
            Transform::IDENTITY
        };
        (framing::pane_camera(anchors), capture)
    });
    let matrix = capture.to_matrix();
    let eye = matrix.transform_point3(record.eye);
    let target = matrix.transform_point3(record.target);
    let forward = (target - eye).normalize_or_zero();
    UnitView {
        root,
        camera: Transform::from_translation(eye).looking_at(
            target,
            Quat::from_axis_angle(forward, record.roll) * Vec3::Y,
        ),
        projection: Projection::custom(framing::pane_projection(record, aspect)),
    }
}

fn orthographic_view(pane: &UnitModelPaneFrame, display_aspect: f32) -> UnitView {
    let width = pane.rect.width();
    let height = pane.rect.height();
    let axis = Quat::from_mat3(&Mat3::from_cols(
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
        Vec3::new(-1.0, 0.0, 0.0),
    ));
    let state = &pane.state;
    let layout_unit = 768.0 * (display_aspect * display_aspect + 1.0).sqrt();
    let position =
        Vec3::new(state.position.0, state.position.1, state.position.2) * layout_unit * pane.scale;
    UnitView {
        root: Transform {
            translation: Vec3::new(-width * 0.5, -height * 0.5, 0.0) + position,
            rotation: axis * Quat::from_rotation_y(state.facing),
            scale: Vec3::splat(1280.0 * state.scale * pane.scale),
        },
        camera: Transform::from_xyz(0.0, 0.0, 1000.0),
        projection: Projection::Orthographic(OrthographicProjection {
            near: 0.1,
            far: 2000.0,
            scaling_mode: ScalingMode::Fixed { width, height },
            ..OrthographicProjection::default_3d()
        }),
    }
}

fn facts_of(model: &M2Model) -> ModelFileFacts {
    ModelFileFacts {
        sequences: model
            .sequences
            .iter()
            .map(|sequence| SequenceFacts {
                anim_id: sequence.anim_id,
                duration_ms: sequence.duration_ms,
                looping: sequence.looping,
            })
            .collect(),
        bbox: model
            .bounds
            .as_ref()
            .map_or(([0.0; 3], [0.0; 3]), |bounds| {
                (bounds.bbox_min, bounds.bbox_max)
            }),
        cameras: model.cameras.len() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use benilla_ui::widget::{FrameKind, WidgetArena};
    use std::collections::HashSet;

    fn widget() -> UnitModelPaneFrame {
        let mut arena = WidgetArena::new();
        UnitModelPaneFrame {
            handle: arena.create(FrameKind::PlayerModel, None, None),
            name: None,
            rect: benilla_ui::layout::Rect::new(0.0, 0.0, 62.0, 62.0),
            scale: 1.0,
            unit: "player".into(),
            guid: 1,
            instance: 1,
            state: benilla_ui::widget::ModelState::default(),
        }
    }

    fn camera() -> PortraitCamera {
        PortraitCamera {
            eye: Vec3::new(0.0, 1.0, -4.0),
            target: Vec3::Y,
            roll: 0.0,
            fov: 0.5,
            near: 0.02,
            far: 100.0,
        }
    }

    fn anchors() -> super::super::PortraitAnchors {
        super::super::PortraitAnchors {
            camera: None,
            pane_camera: Some(camera()),
            bbox_center: Vec3::Y,
            head: Some(Vec3::Y),
            pivot_height: 1.0,
            ground_radius: 0.5,
        }
    }

    fn animations() -> benilla_assets::ModelAnimations {
        let clip = |anim_id, node| benilla_assets::AnimClip {
            anim_id,
            seq_index: 0,
            node: bevy::animation::graph::AnimationNodeIndex::new(node),
            looping: true,
            duration: 1.0,
            move_speed: 0.0,
            blend_time: 0.0,
            bounds_center: Vec3::ZERO,
            bounds_radius: 0.0,
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::ZERO,
            events: Vec::new().into(),
            arm_nodes: None,
            upper_node: None,
            frequency: 1,
            replay: (0, 0),
            poses_bones: true,
        };
        benilla_assets::ModelAnimations {
            graph: Handle::default(),
            clips: vec![clip(0, 1), clip(12, 2)],
            hand_close: [None, None],
            playable_animation_lookup: Vec::new(),
            animation_lookup: Vec::new(),
            global_bones: Vec::new(),
            first_seq: None,
            pose: Default::default(),
        }
    }

    fn pane(handle: FrameHandle, instance: u64, pool: usize) -> UnitPane {
        UnitPane {
            pool,
            instance,
            slot: unit_pane_slot(handle, instance),
            camera: Entity::PLACEHOLDER,
            root: Entity::PLACEHOLDER,
            light: BoothLight::default(),
            scene: None,
            last_pose: None,
            display: None,
            frozen: None,
            play: None,
            clock_ms: 0,
        }
    }

    #[test]
    fn anonymous_copies_have_separate_slots_and_a_rebind_has_no_old_image_key() {
        let mut arena = WidgetArena::new();
        let first = arena.create(FrameKind::PlayerModel, None, None);
        let second = arena.create(FrameKind::PlayerModel, None, None);
        assert_ne!(unit_pane_slot(first, 1), unit_pane_slot(second, 1));
        assert_ne!(unit_pane_slot(first, 1), unit_pane_slot(first, 2));
    }

    #[test]
    fn hiding_rebinding_and_reloading_retire_copies_and_release_the_pool() {
        let mut arena = WidgetArena::new();
        let first = arena.create(FrameKind::PlayerModel, None, None);
        let second = arena.create(FrameKind::PlayerModel, None, None);
        let mut registry = UnitPanes {
            session: 1,
            ..Default::default()
        };
        registry.entries.insert(first, pane(first, 1, 0));
        registry.entries.insert(second, pane(second, 2, 1));
        assert_eq!(registry.free_slot(), Some(2));
        assert_eq!(registry.retire(1, &HashMap::from([(second, 2)])).len(), 1);
        assert_eq!(registry.free_slot(), Some(0));
        assert_eq!(registry.retire(1, &HashMap::from([(second, 3)])).len(), 1);
        registry.entries.insert(second, pane(second, 3, 0));
        assert_eq!(registry.retire(2, &HashMap::from([(second, 3)])).len(), 1);
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn forgetting_a_copy_reaps_its_camera_riders_effect_tree_and_image_bridges() {
        let mut world = World::new();
        let mut queue = bevy::ecs::world::CommandQueue::default();
        let mut arena = WidgetArena::new();
        let handle = arena.create(FrameKind::PlayerModel, None, None);
        let mut booths = Booths::default();
        let mut images = Assets::<Image>::default();
        let copy = {
            let mut commands = Commands::new(&mut queue, &world);
            spawn(handle, 1, 0, &mut commands, &mut booths, &mut images)
        };
        queue.apply(&mut world);
        let (root, camera, slot) = (copy.root, copy.camera, copy.slot.clone());
        let image = booths.0.get(&slot).unwrap().target.clone();
        // Booth riders hang from a rig joint; effect hosts and emitters are children of that
        // same tree, as spawn_booth_effects installs ChildOf(host) after spawn_emitter.
        let joint = world.spawn((Transform::default(), ChildOf(root))).id();
        let rider = world.spawn((Transform::default(), ChildOf(joint))).id();
        let effect = world.spawn((Transform::default(), ChildOf(joint))).id();
        let emitter = world.spawn((Transform::default(), ChildOf(effect))).id();
        let mut portraits = PortraitImages::default();
        portraits
            .0
            .insert(slot.clone(), PortraitSource::Live(image.clone()));
        let mut geometry = BoothPanes::default();
        geometry.0.insert(slot.clone(), 1.0);
        let mut mirrors = RigPaletteMirrors::default();
        {
            let mut commands = Commands::new(&mut queue, &world);
            forget(
                copy,
                &mut commands,
                &mut booths,
                &mut portraits,
                &mut geometry,
                &mut images,
                &mut mirrors,
            );
        }
        queue.apply(&mut world);
        for entity in [root, camera, joint, rider, effect, emitter] {
            assert!(
                world.get_entity(entity).is_err(),
                "the retired tree retained {entity:?}"
            );
        }
        assert!(booths.0.get(&slot).is_none());
        assert!(portraits.0.get(&slot).is_none());
        assert!(geometry.0.get(&slot).is_none());
        assert!(images.get(&image).is_none());
    }

    #[test]
    fn the_view_budget_is_bounded_and_every_live_copy_gets_an_exclusive_layer() {
        let mut arena = WidgetArena::new();
        let mut registry = UnitPanes::default();
        let mut layers = HashSet::new();
        for pool in 0..PANE_LIMIT {
            assert_eq!(registry.free_slot(), Some(pool));
            let handle = arena.create(FrameKind::PlayerModel, None, None);
            registry
                .entries
                .insert(handle, pane(handle, pool as u64 + 1, pool));
            assert!(layers.insert(LAYER_BASE + pool));
        }
        assert!(registry.free_slot().is_none());
        assert!(layers
            .iter()
            .all(|layer| *layer >= UI_MODEL_CAM_LAYER_BASE + UI_MODEL_CAM_LAYERS));
    }

    #[test]
    fn the_default_character_camera_is_frozen_while_later_scale_and_position_move_the_copy() {
        let mut widget = widget();
        let mut frozen = None;
        let first = unit_view(&widget, &anchors(), None, 4.0 / 3.0, &mut frozen);
        widget.state.scale = 2.0;
        widget.state.position = (0.3, 0.2, 0.1);
        widget.state.facing = 0.61;
        let changed = unit_view(&widget, &anchors(), None, 4.0 / 3.0, &mut frozen);
        assert_eq!(first.camera, changed.camera);
        assert_ne!(first.root, changed.root);
        assert_eq!(changed.root.scale, Vec3::splat(2.0));
    }

    #[test]
    fn explicit_file_camera_moves_with_the_copy_and_an_invalid_index_is_orthographic() {
        let mut widget = widget();
        widget.state.unit_camera = Some(0);
        let mut frozen = None;
        let first = unit_view(&widget, &anchors(), Some(camera()), 4.0 / 3.0, &mut frozen);
        widget.state.scale = 2.0;
        let changed = unit_view(&widget, &anchors(), Some(camera()), 4.0 / 3.0, &mut frozen);
        assert!((changed.camera.translation - first.camera.translation * 2.0).length() < 1.0e-5);
        assert!(
            frozen.is_none(),
            "SetCamera installs a live camera, not the ready-hook snapshot"
        );
        widget.state.unit_camera = Some(-1);
        let invalid = unit_view(&widget, &anchors(), None, 4.0 / 3.0, &mut frozen);
        assert!(matches!(invalid.projection, Projection::Orthographic(_)));
    }

    #[test]
    fn the_render_player_samples_the_engines_cursor_instead_of_a_second_animation_clock() {
        let animations = animations();
        let node = |index| bevy::animation::graph::AnimationNodeIndex::new(index);
        let mut player = AnimationPlayer::default();
        player.play(node(2)).repeat();
        sample_pose(
            &mut player,
            &animations,
            Some(ModelPlayHead {
                anim_id: 0,
                cursor_ms: 375,
            }),
        );
        assert_eq!(player.animation(node(1)).unwrap().seek_time(), 0.375);
        assert!(
            player.animation(node(2)).is_none(),
            "a previous native turn cannot keep playing"
        );
        // OnUpdateModel may call SetSequenceTime(0, 375) every paint: it stays at that pose.
        sample_pose(
            &mut player,
            &animations,
            Some(ModelPlayHead {
                anim_id: 0,
                cursor_ms: 375,
            }),
        );
        assert_eq!(player.animation(node(1)).unwrap().seek_time(), 0.375);
        sample_pose(
            &mut player,
            &animations,
            Some(ModelPlayHead {
                anim_id: 12,
                cursor_ms: 40,
            }),
        );
        assert!(player.animation(node(1)).is_none());
        assert_eq!(player.animation(node(2)).unwrap().seek_time(), 0.04);
        sample_pose(&mut player, &animations, None);
        assert!(player.animation(node(2)).is_none());
    }
}
